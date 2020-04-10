// Buttplug Rust Source Code File - See https://buttplug.io for more info.
//
// Copyright 2016-2019 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.

//! Handles client sessions, as well as discovery and communication with hardware.

pub mod comm_managers;
pub mod device_manager;
mod wrapper;
mod logger;

pub use wrapper::{ButtplugInProcessServerWrapper, ButtplugServerWrapper};

use crate::core::{
    errors::*,
    messages::{
        self, ButtplugDeviceCommandMessageUnion, ButtplugDeviceManagerMessageUnion,
        ButtplugMessage, ButtplugInMessage, ButtplugOutMessage, DeviceMessageInfo,
        ButtplugMessageSpecVersion, BUTTPLUG_CURRENT_MESSAGE_SPEC_VERSION
    },
};
use async_std::{
    sync::{channel, Receiver, Sender},
    task,
};
use device_manager::{
    DeviceCommunicationManager, DeviceCommunicationManagerCreator, DeviceManager,
};
use log;
use logger::ButtplugLogHandler;
use std::{
    convert::{TryFrom, TryInto},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

pub enum ButtplugServerEvent {
    DeviceAdded(DeviceMessageInfo),
    DeviceRemoved(DeviceMessageInfo),
    DeviceMessage(ButtplugOutMessage),
    ScanningFinished(),
    ServerError(ButtplugError),
    PingTimeout(),
    Log(messages::Log),
}

struct PingTimer {
    // Needs to be a u128 to compare with Instant, otherwise we have to cast up.
    // This is painful either direction. See
    // https://github.com/rust-lang/rust/issues/58580
    max_ping_time: u128,
    last_ping_time: Arc<RwLock<Instant>>,
    pinged_out: Arc<RwLock<bool>>,
    // This should really be a Condvar but async_std::Condvar isn't done yet, so
    // we'll just use a channel. The channel receiver will get passed to the
    // device manager, so it can stop devices
    ping_channel: Sender<bool>,
}

impl PingTimer {
    pub fn new(max_ping_time: u128) -> (Self, Receiver<bool>) {
        if max_ping_time == 0 {
            panic!("Can't create ping timer with no max ping time.");
        }
        let (sender, receiver) = channel(1);
        (
            Self {
                max_ping_time,
                last_ping_time: Arc::new(RwLock::new(Instant::now())),
                pinged_out: Arc::new(RwLock::new(false)),
                ping_channel: sender,
            },
            receiver,
        )
    }

    pub fn start_ping_timer(&mut self, event_sender: Sender<ButtplugOutMessage>) {
        // Since we've received the handshake, start the ping timer if needed.
        let max_ping_time = self.max_ping_time.clone();
        let last_ping_time = self.last_ping_time.clone();
        let pinged_out = self.pinged_out.clone();
        let ping_channel = self.ping_channel.clone();
        task::spawn(async move {
            loop {
                task::sleep(Duration::from_millis(max_ping_time.try_into().unwrap())).await;
                let last_ping = last_ping_time.read().unwrap().elapsed().as_millis();
                if last_ping > max_ping_time {
                    error!("Pinged out.");
                    *pinged_out.write().unwrap() = true;
                    ping_channel.send(true).await;
                    let err: ButtplugError = ButtplugPingError::new(&format!(
                        "Pinged out. Ping took {} but max ping time is {}.",
                        last_ping, max_ping_time
                    ))
                    .into();
                    event_sender
                        .send(ButtplugOutMessage::Error(err.into()))
                        .await;
                    break;
                }
            }
        });
    }

    pub fn max_ping_time(&self) -> u128 {
        self.max_ping_time
    }

    pub fn update_ping_time(&mut self) {
        *self.last_ping_time.write().unwrap() = Instant::now();
    }

    pub fn pinged_out(&self) -> bool {
        *self.pinged_out.read().unwrap()
    }
}

// TODO Impl Drop for ping timer that stops the internal async task

/// Represents a ButtplugServer.
pub struct ButtplugServer {
    server_name: String,
    server_spec_version: ButtplugMessageSpecVersion,
    client_spec_version: Option<ButtplugMessageSpecVersion>,
    client_name: Option<String>,
    device_manager: DeviceManager,
    event_sender: Sender<ButtplugOutMessage>,
    ping_timer: Option<PingTimer>,
}

impl ButtplugServer {
    pub fn new(
        name: &str,
        max_ping_time: u128
    ) -> (Self, Receiver<ButtplugOutMessage>) {
        let mut ping_timer = None;
        let mut ping_receiver = None;
        let (send, recv) = channel(256);
        if max_ping_time > 0 {
            let (timer, receiver) = PingTimer::new(max_ping_time);
            ping_timer = Some(timer);
            ping_receiver = Some(receiver);
        }
        (Self {
            server_name: name.to_string(),
            server_spec_version: BUTTPLUG_CURRENT_MESSAGE_SPEC_VERSION,
            client_name: None,
            client_spec_version: None,
            device_manager: DeviceManager::new(send.clone(), ping_receiver),
            ping_timer,
            event_sender: send,
        }, recv)
    }

    pub fn add_comm_manager<T>(&mut self)
    where
        T: 'static + DeviceCommunicationManager + DeviceCommunicationManagerCreator,
    {
        self.device_manager.add_comm_manager::<T>();
    }

    pub async fn parse_message(
        &mut self,
        msg: &ButtplugInMessage,
    ) -> Result<ButtplugOutMessage, ButtplugError> {
        if let Some(timer) = &self.ping_timer {
            if timer.pinged_out() {
                return Err(ButtplugPingError::new("Server has pinged out.").into());
            }
        }
        if ButtplugDeviceManagerMessageUnion::try_from(msg.clone()).is_ok()
            || ButtplugDeviceCommandMessageUnion::try_from(msg.clone()).is_ok()
        {
            self.device_manager.parse_message(msg.clone()).await
        } else {
            match msg {
                ButtplugInMessage::RequestServerInfo(ref m) => 
                    self.perform_handshake(m).and_then(|m| Ok(m.into())),
                ButtplugInMessage::Ping(ref p) => 
                    self.handle_ping(p).and_then(|m| Ok(m.into())),
                ButtplugInMessage::RequestLog(ref l) => 
                    self.handle_log(l).and_then(|m| Ok(m.into())),
                _ => Err(ButtplugMessageError::new(
                    &format!("Message {:?} not handled by server loop.", msg).to_owned(),
                )
                .into()),
            }
        }
    }

    fn perform_handshake(
        &mut self,
        msg: &messages::RequestServerInfo,
    ) -> Result<messages::ServerInfo, ButtplugError> {
        if self.server_spec_version < msg.message_version {
            return Err(ButtplugHandshakeError::new(
                &format!(
                    "Server version ({}) must be equal to or greater than client version ({}).",
                    self.server_spec_version, msg.message_version
                )
                .to_owned(),
            )
            .into());
        }
        self.client_name = Some(msg.client_name.clone());
        self.client_spec_version = Some(msg.message_version);
        // Only start the ping timer after we've received the handshake.
        let mut max_ping_time = 0u128;
        if let Some(timer) = &mut self.ping_timer {
            max_ping_time = timer.max_ping_time();
            timer.start_ping_timer(self.event_sender.clone());
        }
        Result::Ok(
            messages::ServerInfo::new(
                &self.server_name,
                self.server_spec_version,
                max_ping_time.try_into().unwrap(),
            )
        )
    }

    fn handle_ping(&mut self, msg: &messages::Ping) -> Result<messages::Ok, ButtplugError> {
        if let Some(timer) = &mut self.ping_timer {
            timer.update_ping_time();
            Result::Ok(messages::Ok::new(msg.get_id()))
        } else {
            Err(
                ButtplugPingError::new("Ping message invalid, as ping timer is not running.")
                    .into(),
            )
        }
    }

    fn handle_log(
        &mut self,
        msg: &messages::RequestLog,
    ) -> Result<messages::Ok, ButtplugError> {
        let handler = ButtplugLogHandler::new(&msg.log_level, self.event_sender.clone());
        log::set_boxed_logger(Box::new(handler))
            .map_err(|e| {
                ButtplugUnknownError::new(&format!("Cannot set up log handler: {}", e)).into()
            })
            .and_then(|_| {
                let level: log::LevelFilter = msg.log_level.clone().into();
                log::set_max_level(level);
                Result::Ok(messages::Ok::new(msg.get_id()))
            })
    }

    // async fn wait_for_event(&self) -> Result<ButtplugServerEvent> {
    // }
}

#[cfg(test)]
mod test {
    use super::*;
    #[cfg(any(feature = "linux-ble", feature = "winrt-ble"))]
    use crate::server::comm_managers::btleplug::BtlePlugCommunicationManager;
    use crate::{
        device::{
            device::{DeviceImplCommand, DeviceWriteCmd},
            Endpoint,
        },
        test::{check_recv_value, TestDevice, TestDeviceCommunicationManager},
    };
    use async_std::{
        prelude::StreamExt,
        sync::Receiver,
        task,
    };
    use std::time::Duration;

    async fn test_server_setup(
        msg_union: &messages::ButtplugInMessage,
    ) -> (ButtplugServer, Receiver<ButtplugOutMessage>) {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut server, recv) = ButtplugServer::new("Test Server", 0);
        assert_eq!(server.server_name, "Test Server");
        match server.parse_message(&msg_union).await.unwrap() {
            ButtplugOutMessage::ServerInfo(_s) => {
                assert_eq!(_s, messages::ServerInfo::new("Test Server", ButtplugMessageSpecVersion::Version2, 0))
            }
            _ => assert!(false, "Should've received ok"),
        }
        (server, recv)
    }

    #[test]
    fn test_server_handshake() {
        let _ = env_logger::builder().is_test(true).try_init();
        let msg = messages::RequestServerInfo::new("Test Client", ButtplugMessageSpecVersion::Version2).into();
        task::block_on(async {
            let (server, _recv) = test_server_setup(&msg).await;
            assert_eq!(server.client_name.unwrap(), "Test Client");
        });
    }

    #[test]
    fn test_server_version_lt() {
        let _ = env_logger::builder().is_test(true).try_init();
        let msg = messages::RequestServerInfo::new("Test Client", ButtplugMessageSpecVersion::Version2).into();
        task::block_on(async {
            test_server_setup(&msg).await;
        });
    }

    // TODO Now that we're moving to a spec version enum, this test is invalid
    // because we can't just pass a u8 in. This should be rebuilt using the
    // JSON parser, and it should fail to deserialize the message.
    #[test]
    #[ignore]
    fn test_server_version_gt() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut server, _) = ButtplugServer::new("Test Server", 0);
        let msg = messages::RequestServerInfo::new("Test Client", ButtplugMessageSpecVersion::Version2).into();
        task::block_on(async {
            assert!(
                server.parse_message(&msg).await.is_err(),
                "Client having higher version than server should fail"
            );
        });
    }

    #[test]
    fn test_ping_timeout() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut server, mut recv) = ButtplugServer::new("Test Server", 100);
        task::block_on(async {
            let msg = messages::RequestServerInfo::new("Test Client", server.server_spec_version);
            task::sleep(Duration::from_millis(150)).await;
            let reply = server.parse_message(&msg.into()).await;
            assert!(
                reply.is_ok(),
                format!(
                    "ping timer shouldn't start until handshake finished. {:?}",
                    reply
                )
            );
            task::sleep(Duration::from_millis(150)).await;
            let pingmsg = messages::Ping::default();
            match server.parse_message(&pingmsg.into()).await {
                Ok(_) => panic!("Should get a ping error back!"),
                Err(e) => {
                    if let ButtplugError::ButtplugPingError(_) = e {
                        // do nothing
                    } else {
                        panic!("Got wrong type of error back!");
                    }
                }
            }
            // Check that we got an event back about the ping out.
            let msg = recv.next().await.unwrap();
            if let ButtplugOutMessage::Error(e) = msg {
                if let ButtplugError::ButtplugPingError(_) = e.into() {
                } else {
                    panic!("Didn't get a ping error");
                }
            } else {
                panic!("Didn't get an error message back");
            }
        });
    }

    #[test]
    #[ignore]
    fn test_device_stop_on_ping_timeout() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut server, mut recv) = ButtplugServer::new("Test Server", 100);
        // TODO This should probably use a test protocol we control, not the aneros protocol
        let (device, device_creator) =
            TestDevice::new_bluetoothle_test_device_impl_creator("Massage Demo");
        TestDeviceCommunicationManager::add_test_device(device_creator);
        server.add_comm_manager::<TestDeviceCommunicationManager>();
        task::block_on(async {
            let msg = messages::RequestServerInfo::new("Test Client", server.server_spec_version);
            let mut reply = server.parse_message(&msg.into()).await;
            assert!(reply.is_ok(), format!("Should get back ok: {:?}", reply));
            reply = server
                .parse_message(&messages::StartScanning::default().into())
                .await;
            assert!(reply.is_ok(), format!("Should get back ok: {:?}", reply));
            // Check that we got an event back about a new device.
            let msg = recv.next().await.unwrap();
            if let ButtplugOutMessage::DeviceAdded(da) = msg {
                assert_eq!(da.device_name, "Aneros Vivi");
            } else {
                assert!(
                    false,
                    format!(
                        "Returned message was not a DeviceAdded message or timed out: {:?}",
                        msg
                    )
                );
            }
            server
                .parse_message(
                    &messages::VibrateCmd::new(0, vec![messages::VibrateSubcommand::new(0, 0.5)])
                        .into(),
                )
                .await
                .unwrap();
            let (_, command_receiver) = device.get_endpoint_channel_clone(&Endpoint::Tx).await;
            check_recv_value(
                &command_receiver,
                DeviceImplCommand::Write(DeviceWriteCmd::new(Endpoint::Tx, vec![0xF1, 63], false)),
            )
            .await;
            // Wait out the ping, we should get a stop message.
            let mut i = 0u32;
            while command_receiver.is_empty() {
                task::sleep(Duration::from_millis(150)).await;
                // Breaks out of loop if we wait for too long.
                i += 1;
                assert!(i < 10, "Slept for too long while waiting for stop command!");
            }
            check_recv_value(
                &command_receiver,
                DeviceImplCommand::Write(DeviceWriteCmd::new(Endpoint::Tx, vec![0xF1, 0], false)),
            )
            .await;
        });
    }

    // Warning: This test is brittle. If any log messages are fired between our
    // log in this message and the asserts, it will fail. If you see failures on
    // this test, that's probably why.
    #[test]
    #[ignore]
    fn test_log_handler() {
        // The log crate only allows one log handler at a time, meaning if we
        // set up env_logger, our server log function won't work. This is a
        // problem. Only uncomment this if this test if failing and you need to
        // see output.
        //
        // let _ = env_logger::builder().is_test(true).try_init();
        let (mut server, mut recv) = ButtplugServer::new("Test Server", 0);
        task::block_on(async {
            let msg = messages::RequestServerInfo::new("Test Client", server.server_spec_version);
            let mut reply = server.parse_message(&msg.into()).await;
            assert!(reply.is_ok(), format!("Should get back ok: {:?}", reply));
            reply = server
                .parse_message(&messages::RequestLog::new(messages::LogLevel::Debug).into())
                .await;
            assert!(reply.is_ok(), format!("Should get back ok: {:?}", reply));
            debug!("Test log message");

            let mut did_log = false;
            // Check that we got an event back about a new device.

            while let Some(msg) = recv.next().await {
                if let ButtplugOutMessage::Log(log) = msg {
                    // We can't assert here, because we may get multiple log
                    // messages back, so we just want to break whenever we get
                    // what we expected.
                    assert_eq!(log.log_level, messages::LogLevel::Debug);
                    assert!(log.log_message.contains("Test log message"));
                    did_log = true;
                    break;
                }
            }

            assert!(did_log, "Should've gotten log message");
        });
    }
}
