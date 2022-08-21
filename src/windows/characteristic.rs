use enumflags2::BitFlags;
use futures::Stream;
use smallvec::SmallVec;
use tokio_stream::StreamExt;
use tracing::warn;
use uuid::Uuid;
use windows::{
    Devices::Bluetooth::{
        BluetoothCacheMode,
        GenericAttributeProfile::{
            GattCharacteristic, GattClientCharacteristicConfigurationDescriptorValue, GattValueChangedEventArgs,
            GattWriteOption, GattWriteResult,
        },
    },
    Foundation::TypedEventHandler,
    Storage::Streams::{DataReader, DataWriter},
};

use crate::{error::ErrorKind, CharacteristicProperty, Error, Result};

use super::{descriptor::Descriptor, error::check_communication_status};

/// A Bluetooth GATT characteristic
#[derive(Clone, PartialEq, Eq)]
pub struct Characteristic {
    inner: GattCharacteristic,
}

impl std::fmt::Debug for Characteristic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Characteristic")
            .field("uuid", &self.inner.Uuid().expect("UUID missing on GattCharacteristic"))
            .field(
                "handle",
                &self
                    .inner
                    .AttributeHandle()
                    .expect("AttributeHandle missing on GattCharacteristic"),
            )
            .finish()
    }
}

impl Characteristic {
    pub(super) fn new(characteristic: GattCharacteristic) -> Self {
        Characteristic { inner: characteristic }
    }

    /// The [Uuid] identifying the type of this GATT characteristic
    pub fn uuid(&self) -> Uuid {
        Uuid::from_u128(self.inner.Uuid().expect("UUID missing on GattCharacteristic").to_u128())
    }

    /// The properties of this this GATT characteristic.
    ///
    /// Characteristic properties indicate which operations (e.g. read, write, notify, etc) may be performed on this
    /// characteristic.
    pub fn properties(&self) -> BitFlags<CharacteristicProperty> {
        let props = self
            .inner
            .CharacteristicProperties()
            .expect("CharacteristicProperties missing on GattCharacteristic");
        BitFlags::from_bits(props.0).unwrap_or_else(|e| e.truncate())
    }

    /// The cached value of this characteristic
    ///
    /// If the value has not yet been read, this function may either return an error or perform a read of the value.
    pub async fn value(&self) -> Result<SmallVec<[u8; 16]>> {
        self.read_value(BluetoothCacheMode::Cached).await
    }

    /// Read the value of this characteristic from the device
    pub async fn read(&self) -> Result<SmallVec<[u8; 16]>> {
        self.read_value(BluetoothCacheMode::Uncached).await
    }

    async fn read_value(&self, cachemode: BluetoothCacheMode) -> Result<SmallVec<[u8; 16]>> {
        let res = self.inner.ReadValueWithCacheModeAsync(cachemode)?.await?;

        check_communication_status(res.Status()?, res.ProtocolError(), "reading characteristic")?;

        let buf = res.Value()?;
        let mut data = SmallVec::from_elem(0, buf.Length()? as usize);
        let reader = DataReader::FromBuffer(&buf)?;
        reader.ReadBytes(data.as_mut_slice())?;
        Ok(data)
    }

    /// Write the value of this descriptor on the device to `value`
    pub async fn write(&self, value: &[u8]) -> Result<()> {
        self.write_kind(value, GattWriteOption::WriteWithoutResponse).await
    }

    /// Write the value of this descriptor on the device to `value` and request the device return a response indicating
    /// a successful write.
    pub async fn write_with_response(&self, value: &[u8]) -> Result<()> {
        self.write_kind(value, GattWriteOption::WriteWithResponse).await
    }

    async fn write_kind(&self, value: &[u8], writeoption: GattWriteOption) -> Result<()> {
        let writer = DataWriter::new()?;
        writer.WriteBytes(value)?;
        let buf = writer.DetachBuffer()?;
        let res = self
            .inner
            .WriteValueWithResultAndOptionAsync(&buf, writeoption)?
            .await?;

        check_communication_status(res.Status()?, res.ProtocolError(), "writing characteristic")
    }

    /// Enables notification of value changes for this GATT characteristic.
    ///
    /// Returns a stream of values for the characteristic sent from the device.
    pub async fn notify(&self) -> Result<impl Stream<Item = Result<SmallVec<[u8; 16]>>> + '_> {
        let props = self.properties();
        let value = if props.contains(CharacteristicProperty::Notify) {
            GattClientCharacteristicConfigurationDescriptorValue::Notify
        } else if props.contains(CharacteristicProperty::Indicate) {
            GattClientCharacteristicConfigurationDescriptorValue::Indicate
        } else {
            return Err(Error::new(
                ErrorKind::NotSupported,
                None,
                "characteristic does not support indications or notifications".to_string(),
            ));
        };

        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        let token = self.inner.ValueChanged(&TypedEventHandler::new(
            move |_characteristic, event_args: &Option<GattValueChangedEventArgs>| {
                let event_args = event_args
                    .as_ref()
                    .expect("GattValueChangedEventArgs was null in ValueChanged handler");

                fn get_value(event_args: &GattValueChangedEventArgs) -> Result<SmallVec<[u8; 16]>> {
                    let buf = event_args.CharacteristicValue()?;
                    let len = buf.Length()?;
                    let mut data: SmallVec<[u8; 16]> = SmallVec::from_elem(0, len as usize);
                    let reader = DataReader::FromBuffer(&buf)?;
                    reader.ReadBytes(data.as_mut_slice())?;
                    Ok(data)
                }

                let _ = sender.blocking_send(get_value(event_args));

                Ok(())
            },
        ))?;

        let guard = scopeguard::guard((), move |_| {
            if let Err(err) = self.inner.RemoveValueChanged(token) {
                warn!("Error removing value change event handler: {:?}", err);
            }
        });

        let res = self
            .inner
            .WriteClientCharacteristicConfigurationDescriptorWithResultAsync(value)?
            .await?;

        check_communication_status(res.Status()?, res.ProtocolError(), "enabling notifications")?;

        let guard = scopeguard::guard((), move |_| {
            let _guard = guard;
            match self
                .inner
                .WriteClientCharacteristicConfigurationDescriptorWithResultAsync(
                    GattClientCharacteristicConfigurationDescriptorValue::None,
                ) {
                Ok(fut) => {
                    tokio::task::spawn(async move {
                        fn check_status(res: windows::core::Result<GattWriteResult>) -> Result<()> {
                            let res = res?;
                            check_communication_status(
                                res.Status()?,
                                res.ProtocolError(),
                                "disabling characteristic notifications",
                            )
                        }

                        if let Err(err) = check_status(fut.await) {
                            warn!("{:?}", err);
                        }
                    });
                }
                Err(err) => {
                    warn!("Error disabling characteristic notifications: {:?}", err);
                }
            }
        });

        Ok(tokio_stream::wrappers::ReceiverStream::new(receiver).map(move |x| {
            let _guard = &guard;
            x
        }))
    }

    /// Is the device currently sending notifications for this characteristic?
    pub async fn is_notifying(&self) -> Result<bool> {
        let res = self
            .inner
            .ReadClientCharacteristicConfigurationDescriptorAsync()?
            .await?;

        check_communication_status(
            res.Status()?,
            res.ProtocolError(),
            "reading client characteristic configuration descriptor",
        )?;

        const INDICATE: i32 = GattClientCharacteristicConfigurationDescriptorValue::Indicate.0;
        const NOTIFY: i32 = GattClientCharacteristicConfigurationDescriptorValue::Notify.0;
        let cccd = res.ClientCharacteristicConfigurationDescriptor()?;
        Ok((cccd.0 & (INDICATE | NOTIFY)) != 0)
    }

    /// Discover the descriptors associated with this characteristic.
    pub async fn discover_descriptors(&self) -> Result<SmallVec<[Descriptor; 2]>> {
        self.get_descriptors(BluetoothCacheMode::Uncached).await
    }

    /// Get previously discovered descriptors.
    ///
    /// If no descriptors have been discovered yet, this function may either perform descriptor discovery or
    /// return an empty set.
    pub async fn descriptors(&self) -> Result<SmallVec<[Descriptor; 2]>> {
        self.get_descriptors(BluetoothCacheMode::Cached).await
    }

    async fn get_descriptors(&self, cachemode: BluetoothCacheMode) -> Result<SmallVec<[Descriptor; 2]>> {
        let res = self.inner.GetDescriptorsWithCacheModeAsync(cachemode)?.await?;
        check_communication_status(res.Status()?, res.ProtocolError(), "discovering descriptors")?;
        let descriptors = res.Descriptors()?;
        Ok(descriptors.into_iter().map(Descriptor::new).collect())
    }
}
