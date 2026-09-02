//! `IMMNotificationClient` — device hot-plug handling.
//!
//! Agents unplug USB headsets constantly: to take a break, to answer a personal call,
//! to move desks between shifts. Losing an in-progress call to a replug is not
//! acceptable, so the capture loop re-resolves the pinned device on every state
//! change and reopens the stream rather than waiting for the next call.
//!
//! Note that `OnDefaultDeviceChanged` is handled but never acted on as a *selection*
//! signal — the pinned device is the only device we capture. It matters only because
//! a default change often accompanies the endpoint churn we do care about.

use crate::device::{DeviceEvent, DeviceId, Direction};
use std::sync::mpsc::Sender;
use windows::core::{implement, PCWSTR};
use windows::Win32::Media::Audio::{
    eCapture, eRender, EDataFlow, ERole, IMMNotificationClient, IMMNotificationClient_Impl,
    DEVICE_STATE, DEVICE_STATE_ACTIVE,
};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

#[implement(IMMNotificationClient)]
pub struct DeviceNotificationClient {
    tx: Sender<DeviceEvent>,
}

impl DeviceNotificationClient {
    pub fn new(tx: Sender<DeviceEvent>) -> Self {
        DeviceNotificationClient { tx }
    }
}

fn direction_of(flow: EDataFlow) -> Direction {
    if flow == eCapture {
        Direction::Capture
    } else {
        Direction::Render
    }
}

fn id_of(id: &PCWSTR) -> DeviceId {
    DeviceId(unsafe { id.to_string() }.unwrap_or_default())
}

#[allow(non_snake_case)]
impl IMMNotificationClient_Impl for DeviceNotificationClient_Impl {
    fn OnDeviceStateChanged(
        &self,
        pwstrdeviceid: &PCWSTR,
        dwnewstate: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        let _ = self.tx.send(DeviceEvent::StateChanged {
            id: id_of(pwstrdeviceid),
            active: dwnewstate == DEVICE_STATE_ACTIVE,
        });
        Ok(())
    }

    fn OnDeviceAdded(&self, pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
        // The full device description needs a property-store read, which must not
        // happen on this callback thread: the audio engine holds a lock while it
        // fires notifications and a re-entrant enumeration deadlocks. The capture
        // loop re-enumerates when it sees the event.
        let _ = self.tx.send(DeviceEvent::StateChanged {
            id: id_of(pwstrdeviceid),
            active: true,
        });
        Ok(())
    }

    fn OnDeviceRemoved(&self, pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
        let _ = self.tx.send(DeviceEvent::Removed(id_of(pwstrdeviceid)));
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        _role: ERole,
        pwstrdefaultdeviceid: &PCWSTR,
    ) -> windows::core::Result<()> {
        let _ = self.tx.send(DeviceEvent::DefaultChanged {
            id: id_of(pwstrdefaultdeviceid),
            direction: direction_of(flow),
        });
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _pwstrdeviceid: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Register a notification client on the enumerator.
///
/// The returned interface must be kept alive for as long as notifications are wanted,
/// and unregistered before the enumerator is released — otherwise the audio service
/// keeps calling into freed memory when the process exits.
pub fn register(
    enumerator: &windows::Win32::Media::Audio::IMMDeviceEnumerator,
    tx: Sender<DeviceEvent>,
) -> windows::core::Result<IMMNotificationClient> {
    let client: IMMNotificationClient = DeviceNotificationClient::new(tx).into();
    unsafe { enumerator.RegisterEndpointNotificationCallback(&client)? };
    let _ = eRender;
    Ok(client)
}
