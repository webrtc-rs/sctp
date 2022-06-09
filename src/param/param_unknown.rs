use std::any::Any;
use std::fmt::{Debug, Display, Formatter};
use bytes::{Bytes, BytesMut};
use crate::param::Param;
use crate::param::param_header::ParamHeader;
use crate::param::param_type::ParamType;
use crate::param::param_header::PARAM_HEADER_LENGTH;

#[derive(Clone, Debug, PartialEq)]
pub struct ParamUnknown {
    typ: u16,
    value: Bytes,
}

impl Display for ParamUnknown {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "ParamUnknown( {} {:?} )", self.header(), self.value)
    }
}

impl Param for ParamUnknown {
    fn header(&self) -> ParamHeader {
        ParamHeader {
            typ: ParamType::Unknown { param_type: self.typ },
            value_length: self.value.len() as u16,
        }
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        &*self
    }

    fn unmarshal(raw: &Bytes) -> crate::error::Result<Self> where Self: Sized {
        let header = ParamHeader::unmarshal(raw)?;
        let value = raw.slice(PARAM_HEADER_LENGTH..PARAM_HEADER_LENGTH + header.value_length());
        Ok(Self {
            typ: header.typ.into(),
            value,
        })
    }

    fn marshal_to(&self, buf: &mut BytesMut) -> crate::error::Result<usize> {
        self.header().marshal_to(buf)?;
        buf.extend(self.value.clone());
        Ok(buf.len())
    }

    fn value_length(&self) -> usize {
        self.value.len()
    }

    fn clone_to(&self) -> Box<dyn Param + Send + Sync> {
        Box::new(self.clone())
    }
}