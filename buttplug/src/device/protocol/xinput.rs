use super::{ButtplugDeviceResultFuture, ButtplugProtocol, ButtplugProtocolCommandHandler};
use crate::{
  core::{
    errors::ButtplugMessageError,
    messages::{self, ButtplugDeviceCommandMessageUnion, MessageAttributesMap},
  },
  device::{
    protocol::{generic_command_manager::GenericCommandManager, ButtplugProtocolProperties},
    DeviceImpl,
    DeviceWriteCmd,
    Endpoint,
  },
};
use async_mutex::Mutex;
use byteorder::{LittleEndian, WriteBytesExt};
use std::sync::Arc;

#[derive(ButtplugProtocolProperties)]
pub struct XInput {
  name: String,
  message_attributes: MessageAttributesMap,
  manager: Arc<Mutex<GenericCommandManager>>,
  stop_commands: Vec<ButtplugDeviceCommandMessageUnion>,
}

impl ButtplugProtocol for XInput {
  fn new_protocol(
    name: &str,
    message_attributes: MessageAttributesMap,
  ) -> Box<dyn ButtplugProtocol> {
    let manager = GenericCommandManager::new(&message_attributes);

    Box::new(Self {
      name: name.to_owned(),
      message_attributes,
      stop_commands: manager.get_stop_commands(),
      manager: Arc::new(Mutex::new(manager)),
    })
  }
}

impl ButtplugProtocolCommandHandler for XInput {
  fn handle_vibrate_cmd(
    &self,
    device: Arc<Box<dyn DeviceImpl>>,
    msg: messages::VibrateCmd,
  ) -> ButtplugDeviceResultFuture {
    let manager = self.manager.clone();
    Box::pin(async move {
      // Store off result before the match, so we drop the lock ASAP.
      let result = manager.lock().await.update_vibration(&msg, true);
      // My life for an async closure so I could just do this via and_then(). :(
      match result {
        Ok(cmds_option) => {
          let mut fut_vec = vec![];
          if let Some(cmds) = cmds_option {
            // XInput is fast enough that we can ignore the commands handed
            // back by the manager and just form our own packet. This means
            // we'll just use the manager's return for command validity
            // checking.
            let mut cmd = vec![];
            if cmd
              .write_u16::<LittleEndian>(cmds[1].unwrap() as u16)
              .is_err()
              || cmd
                .write_u16::<LittleEndian>(cmds[0].unwrap() as u16)
                .is_err()
            {
              return Err(
                ButtplugMessageError::MessageConversionError(
                  "Cannot convert XInput value for processing",
                )
                .into(),
              );
            }
            fut_vec.push(device.write_value(DeviceWriteCmd::new(Endpoint::Tx, cmd, false)));
          }

          for fut in fut_vec {
            fut.await?;
          }
          Ok(messages::Ok::default().into())
        }
        Err(e) => Err(e),
      }
    })
  }
}
