use bytes::Bytes;

/// Reference-counted encoded bytes shared between pipeline stages.
///
/// Cloning is a refcount bump, so one buffer travels from the demuxer through
/// muxing into every concurrent HLS reader without being copied. Muxers should
/// build into a [`bytes::BytesMut`] and freeze it, which hands ownership over
/// without a final copy either.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Payload(Bytes);

impl Payload {
    pub fn from_bytes(bytes: impl Into<Bytes>) -> Self {
        Self(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn bytes(&self) -> &Bytes {
        &self.0
    }

    pub fn into_bytes(self) -> Bytes {
        self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<u8>> for Payload {
    fn from(bytes: Vec<u8>) -> Self {
        Self(Bytes::from(bytes))
    }
}

impl From<&'static [u8]> for Payload {
    fn from(bytes: &'static [u8]) -> Self {
        Self(Bytes::from_static(bytes))
    }
}
