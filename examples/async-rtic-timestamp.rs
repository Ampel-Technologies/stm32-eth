#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

//! For build and run instructions, see README.md
//!
//! This example is used to demonstrate async abilities, and requires nightly to build.
//!
//! A very rudimentary PTP synchronization example built using RTIC, on the async branch.
//!
//! The example requires that at least two nodes are running at the same time,
//! and the time synchronization that occurs does not explicitly compensate for
//! network delays.
//!
//! All nodes send traffic to a specific MAC address (AB:CD:EF:12:34:56) with an unused
//! EtherType (0xFFFF), containing nothing but the raw value of a [`Timestamp`]. Upon reception
//! of such a frame, the node will parse the timestamp, compare it to when the frame was received
//! according to the local time, and do one of following:
//!
//! 1. If the difference is larger than 20 microseconds, the current local time is set to the
//!    received value.
//! 2. If the difference is smaller than or equal to 20 microseconds, the PTP addend value is updated
//!    to compensate for the observed difference.
//!
//! When using the internal oscillator of an STM32, step 2 will (almost) never occur, as the frequency
//! drift and error with this clock is too great to accurately compensate for. However,
//! if a more accurate High Speed External oscillator is connected to your MCU, even this very basic
//! synchronization scheme can synchronize the rate of time on two nodes to within a few PPMs.
//!
//! To activate the HSE configuration for the examples, set the `STM32_ETH_EXAMPLE_HSE` environment variable
//! to `oscillator` or `bypass` when compiling examples.

use defmt_rtt as _;
use panic_probe as _;

mod common;

extern crate async_rtic as rtic;

#[rtic::app(device = stm32_eth::stm32, dispatchers = [SPI1])]
mod app {

    use async_rtic as rtic;

    use crate::common::EthernetPhy;

    use fugit::ExtU64;

    use arbiter::Arbiter;

    use ieee802_3_miim::{phy::PhySpeed, Phy};
    use systick_monotonic::Systick;

    use stm32_eth::{
        dma::{EthernetDMA, PacketId, RxRing, RxRingEntry, TxRing, TxRingEntry},
        mac::Speed,
        ptp::{EthernetPTP, Subseconds, Timestamp},
        Parts,
    };

    use core::mem::MaybeUninit;

    #[local]
    struct Local {}

    #[shared]
    struct Shared {}

    #[monotonic(binds = SysTick, default = true)]
    type Monotonic = Systick<1000>;

    #[init(local = [
        rx_ring: [RxRingEntry; 2] = [RxRingEntry::new(),RxRingEntry::new()],
        tx_ring: [TxRingEntry; 2] = [TxRingEntry::new(),TxRingEntry::new()],
        dma: MaybeUninit<EthernetDMA<'static, 'static>> = MaybeUninit::uninit(),
        arbiter: MaybeUninit<Arbiter<EthernetPTP> > = MaybeUninit::uninit(),
    ])]
    fn init(cx: init::Context) -> (Shared, Local, init::Monotonics) {
        defmt::info!("Pre-init");
        let core = cx.core;
        let p = cx.device;

        let rx_ring = cx.local.rx_ring;
        let tx_ring = cx.local.tx_ring;

        let (clocks, gpio, ethernet) = crate::common::setup_peripherals(p);
        let mono = Systick::new(core.SYST, clocks.hclk().raw());

        defmt::info!("Setting up pins");
        let (pins, mdio, mdc, pps) = crate::common::setup_pins(gpio);

        defmt::info!("Configuring ethernet");

        let Parts { dma, mac, mut ptp } =
            stm32_eth::new_with_mii(ethernet, rx_ring, tx_ring, clocks, pins, mdio, mdc).unwrap();

        let dma = cx.local.dma.write(dma);

        ptp.enable_pps(pps);

        let arbiter = cx.local.arbiter.write(Arbiter::new(ptp));

        defmt::info!("Enabling interrupts");
        dma.enable_interrupt();

        let (rx, tx) = dma.split();

        match EthernetPhy::from_miim(mac, 0) {
            Ok(mut phy) => {
                defmt::info!(
                    "Resetting PHY as an extra step. Type: {}",
                    phy.ident_string()
                );

                phy.phy_init();

                defmt::info!("Waiting for link up.");

                while !phy.phy_link_up() {}

                defmt::info!("Link up.");

                if let Some(speed) = phy.speed().map(|s| match s {
                    PhySpeed::HalfDuplexBase10T => Speed::HalfDuplexBase10T,
                    PhySpeed::FullDuplexBase10T => Speed::FullDuplexBase10T,
                    PhySpeed::HalfDuplexBase100Tx => Speed::HalfDuplexBase100Tx,
                    PhySpeed::FullDuplexBase100Tx => Speed::FullDuplexBase100Tx,
                }) {
                    phy.get_miim().set_speed(speed);
                    defmt::info!("Detected link speed: {}", speed);
                } else {
                    defmt::warn!("Failed to detect link speed.");
                }
            }
            Err(_) => {
                defmt::info!("Not resetting unsupported PHY. Cannot detect link speed.");
            }
        };

        sender::spawn(tx).ok();
        receiver::spawn(rx, arbiter).ok();
        ptp_scheduler::spawn(arbiter).ok();

        (Shared {}, Local {}, init::Monotonics(mono))
    }

    #[task]
    async fn receiver(
        _: receiver::Context,
        rx: &'static mut RxRing<'static>,
        ptp: &'static Arbiter<EthernetPTP>,
    ) {
        let mut packet_id_counter = 0;
        loop {
            let packet_id = PacketId(packet_id_counter);
            let packet = rx.recv(Some(packet_id.clone())).await;

            let dst_mac = &packet[0..6];

            let rx_timestamp = if let Some(timestamp) = packet.timestamp() {
                timestamp
            } else {
                continue;
            };

            defmt::debug!("RX timestamp: {}", rx_timestamp);

            if dst_mac == [0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56] {
                let mut timestamp_data = [0u8; 8];
                timestamp_data.copy_from_slice(&packet[14..22]);
                let raw = i64::from_be_bytes(timestamp_data);

                let timestamp = Timestamp::new_raw(raw);

                defmt::debug!("Contained TX timestamp: {}", rx_timestamp);

                let diff = timestamp - rx_timestamp;

                defmt::info!("Difference between TX and RX time: {}", diff);

                let mut ptp = ptp.access().await;
                let addend = ptp.addend();
                let nanos = diff.nanos() as u64;

                if nanos <= 20_000 {
                    let p1 = ((nanos * addend as u64) / 1_000_000_000) as u32;

                    defmt::debug!("Addend correction value: {}", p1);

                    if diff.is_negative() {
                        ptp.set_addend(addend - p1 / 2);
                    } else {
                        ptp.set_addend(addend + p1 / 2);
                    };
                } else {
                    defmt::warn!("Updated time.");
                    ptp.update_time(diff);
                }
                drop(ptp);
            }

            drop(packet);

            let polled_ts = rx.timestamp(&packet_id);

            assert_eq!(polled_ts, Ok(Some(rx_timestamp)));

            packet_id_counter += 1;
            packet_id_counter &= !0x8000_0000;
        }
    }

    #[task]
    async fn ptp_scheduler(_: ptp_scheduler::Context, ptp: &'static Arbiter<EthernetPTP>) {
        loop {
            let mut ptp = ptp.access().await;
            let start = EthernetPTP::now();
            let int_time = start + Timestamp::new_raw(Subseconds::MAX_VALUE as i64);
            ptp.wait_until(int_time).await;
            let now = EthernetPTP::now();

            defmt::info!("Got to PTP time after {}.", now - start);
        }
    }

    #[task]
    async fn sender(_: sender::Context, tx: &'static mut TxRing<'static>) {
        let mut tx_id_ctr = 0x8000_0000;

        const SIZE: usize = 42;

        loop {
            // Obtain the current time to use as the "TX time" of our frame. It is clearly
            // incorrect, but works well enough in low-activity systems (such as this example).
            let now = EthernetPTP::now();
            let start = monotonics::now();

            const DST_MAC: [u8; 6] = [0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56];
            const SRC_MAC: [u8; 6] = [0x00, 0x00, 0xDE, 0xAD, 0xBE, 0xEF];
            const ETH_TYPE: [u8; 2] = [0xFF, 0xFF]; // Custom/unknown ethertype

            let tx_id_val = tx_id_ctr;
            let packet_id = PacketId(tx_id_val);
            let mut tx_buffer = tx.prepare_packet(SIZE, Some(packet_id.clone())).await;
            // Write the Ethernet Header and the current timestamp value to
            // the frame.
            tx_buffer[0..6].copy_from_slice(&DST_MAC);
            tx_buffer[6..12].copy_from_slice(&SRC_MAC);
            tx_buffer[12..14].copy_from_slice(&ETH_TYPE);
            tx_buffer[14..22].copy_from_slice(&now.raw().to_be_bytes());

            tx_buffer.send();

            tx_id_ctr += 1;
            tx_id_ctr |= 0x8000_0000;

            let timestamp = tx.timestamp(&packet_id).await;

            if let Ok(Some(timestamp)) = timestamp {
                defmt::info!("Tx timestamp: {}", timestamp);
            }

            monotonics::delay_until(start + 500.millis()).await;
        }
    }

    #[task(binds = ETH, priority = 2)]
    fn eth_interrupt(_: eth_interrupt::Context) {
        stm32_eth::eth_interrupt_handler();
    }
}
