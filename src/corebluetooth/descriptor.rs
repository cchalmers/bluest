use objc::{class, msg_send, sel, sel_impl};
use objc_foundation::{INSData, INSObject, NSObject};
use objc_id::ShareId;

use super::delegates::{PeripheralDelegate, PeripheralEvent};
use super::types::{CBDescriptor, CBPeripheralState, NSUInteger};
use crate::error::ErrorKind;
use crate::{Descriptor, Error, Result, Uuid};

/// A Bluetooth GATT descriptor
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptorImpl {
    inner: ShareId<CBDescriptor>,
    delegate: ShareId<PeripheralDelegate>,
}

fn value_to_slice(val: &NSObject) -> Vec<u8> {
    if val.is_kind_of(class!(NSNumber)) {
        // Characteristic EXtended Properties, Client Characteristic COnfiguration, Service Characteristic Configuration, or L2CAP PSM Value Characteristic
        let n: u16 = unsafe { msg_send![val, unsignedShortValue] };
        n.to_le_bytes().to_vec()
    } else if val.is_kind_of(class!(NSString)) {
        // Characteristic User Description
        let ptr: *const u8 = unsafe { msg_send![val, UTF8String] };
        let val = if ptr.is_null() {
            &[]
        } else {
            let len: NSUInteger = unsafe { msg_send![val, lengthOfBytesUsingEncoding: 4usize] }; // NSUTF8StringEncoding
            unsafe { std::slice::from_raw_parts(ptr, len) }
        };
        val.to_vec()
    } else if val.is_kind_of(class!(NSData)) {
        // All other descriptors
        let ptr: *const u8 = unsafe { msg_send![val, bytes] };
        let val = if ptr.is_null() {
            &[]
        } else {
            let len: NSUInteger = unsafe { msg_send![val, length] };
            unsafe { std::slice::from_raw_parts(ptr, len) }
        };
        val.to_vec()
    } else {
        Vec::new()
    }
}

impl Descriptor {
    pub(super) fn new(descriptor: &CBDescriptor) -> Self {
        let characteristic = descriptor.characteristic();
        let service = characteristic.service().expect("service should exist");
        let peripheral = service.peripheral();
        let delegate = peripheral
            .delegate()
            .expect("the peripheral should have a delegate attached");

        Descriptor(DescriptorImpl {
            inner: unsafe { ShareId::from_ptr(descriptor as *const _ as *mut _) },
            delegate,
        })
    }
}

impl DescriptorImpl {
    /// The [`Uuid`] identifying the type of this GATT descriptor
    pub fn uuid(&self) -> Uuid {
        self.inner.uuid().to_uuid()
    }

    /// The [`Uuid`] identifying the type of this GATT descriptor
    pub async fn uuid_async(&self) -> Result<Uuid> {
        Ok(self.uuid())
    }

    /// The cached value of this descriptor
    ///
    /// If the value has not yet been read, this method may either return an error or perform a read of the value.
    pub async fn value(&self) -> Result<Vec<u8>> {
        self.inner.value().map(|val| value_to_slice(&val)).ok_or_else(|| {
            Error::new(
                ErrorKind::NotReady,
                None,
                "the descriptor value has not been read".to_string(),
            )
        })
    }

    /// Read the value of this descriptor from the device
    pub async fn read(&self) -> Result<Vec<u8>> {
        let service = self.inner.characteristic().service().expect("service should exist");
        let peripheral = service.peripheral();
        let mut receiver = self.delegate.sender().subscribe();

        if peripheral.state() != CBPeripheralState::CONNECTED {
            return Err(ErrorKind::NotConnected.into());
        }

        peripheral.read_descriptor_value(&self.inner);

        loop {
            match receiver.recv().await.map_err(Error::from_recv_error)? {
                PeripheralEvent::DescriptorValueUpdate { descriptor, error } if descriptor == self.inner => match error
                {
                    Some(err) => return Err(Error::from_nserror(err)),
                    None => return self.value().await,
                },
                PeripheralEvent::Disconnected { error } => {
                    return Err(Error::from_kind_and_nserror(ErrorKind::NotConnected, error));
                }
                PeripheralEvent::ServicesChanged { invalidated_services }
                    if invalidated_services.contains(&service) =>
                {
                    return Err(ErrorKind::ServiceChanged.into());
                }
                _ => (),
            }
        }
    }

    /// Write the value of this descriptor on the device to `value`
    pub async fn write(&self, value: &[u8]) -> Result<()> {
        let service = match self.inner.characteristic().service() {
            Some(service) => service,
            None => return Err(ErrorKind::NotReady.into()),
        };
        let peripheral = service.peripheral();
        let mut receiver = self.delegate.sender().subscribe();

        if peripheral.state() != CBPeripheralState::CONNECTED {
            return Err(ErrorKind::NotConnected.into());
        }

        let data = INSData::from_vec(value.to_vec());
        peripheral.write_descriptor_value(&self.inner, &data);

        loop {
            match receiver.recv().await.map_err(Error::from_recv_error)? {
                PeripheralEvent::DescriptorValueWrite { descriptor, error } if descriptor == self.inner => {
                    match error {
                        Some(err) => return Err(Error::from_nserror(err)),
                        None => return Ok(()),
                    }
                }
                PeripheralEvent::Disconnected { error } => {
                    return Err(Error::from_kind_and_nserror(ErrorKind::NotConnected, error));
                }
                PeripheralEvent::ServicesChanged { invalidated_services }
                    if invalidated_services.contains(&service) =>
                {
                    return Err(ErrorKind::ServiceChanged.into());
                }
                _ => (),
            }
        }
    }
}
