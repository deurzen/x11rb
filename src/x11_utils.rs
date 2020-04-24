use std::convert::{TryFrom, TryInto};

use crate::errors::ParseError;

/// Information about a X11 extension.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ExtensionInformation {
    /// Major opcode used in request
    pub major_opcode: u8,
    /// Lowest event number used by the extension.
    pub first_event: u8,
    /// Lowest error number used by the extension.
    pub first_error: u8,
}

/// Trait to provide information about extensions.
pub trait ExtInfoProvider {
    /// Returns the information of the extension that whose
    /// opcode is `major_opcode`.
    fn get_from_major_opcode(&self, major_opcode: u8) -> Option<(&str, ExtensionInformation)>;

    /// Returns the information of the extension that whose
    /// event number range includes `event_number`.
    fn get_from_event_code(&self, event_code: u8) -> Option<(&str, ExtensionInformation)>;

    /// Returns the information of the extension that whose
    /// error number range includes `error_number`.
    fn get_from_error_code(&self, error_code: u8) -> Option<(&str, ExtensionInformation)>;
}

/// Common information on events and errors.
///
/// This trait exists to share some code between `GenericEvent` and `GenericError`.
pub trait Event {
    /// Provide the raw data of the event as a slice.
    fn raw_bytes(&self) -> &[u8];

    /// The raw type of this response.
    ///
    /// Response types have seven bits in X11. The eight bit indicates whether this packet was
    /// generated through the `SendEvent` request.
    ///
    /// See also the `response_type()` and `server_generated()` methods which decompose this field
    /// into the contained information.
    fn raw_response_type(&self) -> u8 {
        self.raw_bytes()[0]
    }

    /// The type of this response.
    ///
    /// All errors have a response type of 0. Replies have a response type of 1, but you should
    /// never see their raw bytes in your code. Other response types are provided as constants in
    /// the generated code. Note that extensions have their response type dynamically assigned.
    fn response_type(&self) -> u8 {
        self.raw_response_type() & 0x7f
    }

    /// Was this packet generated by the server?
    ///
    /// If this function returns true, then this event comes from the X11 server. Otherwise, it was
    /// sent from another client via the `SendEvent` request.
    fn server_generated(&self) -> bool {
        self.raw_response_type() & 0x80 == 0
    }

    /// Get the sequence number of this packet.
    ///
    /// Not all packets contain a sequence number, so this function returns an `Option`.
    fn raw_sequence_number(&self) -> Option<u16> {
        use crate::protocol::xproto::KEYMAP_NOTIFY_EVENT;
        match self.response_type() {
            KEYMAP_NOTIFY_EVENT => None,
            _ => {
                let bytes = self.raw_bytes();
                Some(u16::from_ne_bytes([bytes[2], bytes[3]]))
            }
        }
    }
}

/// A generic event.
///
/// Examine the event's `response_type()` and use `TryInto::try_into()` to convert the event to the
/// desired type.
#[derive(Debug, Clone)]
pub struct GenericEvent<B: AsRef<[u8]>>(B);

impl<B: AsRef<[u8]>> GenericEvent<B> {
    pub fn new(value: B) -> Result<Self, ParseError> {
        use super::protocol::xproto::GE_GENERIC_EVENT;
        let value_slice = value.as_ref();
        if value_slice.len() < 32 {
            return Err(ParseError::ParseError);
        }
        let length_field = u32::from_ne_bytes([
            value_slice[4],
            value_slice[5],
            value_slice[6],
            value_slice[7],
        ]);
        let length_field: usize = length_field.try_into().or(Err(ParseError::ParseError))?;
        let actual_length = value_slice.len();
        let event = GenericEvent(value);
        let expected_length = match event.response_type() {
            GE_GENERIC_EVENT | REPLY => 32 + 4 * length_field,
            _ => 32,
        };
        if actual_length != expected_length {
            return Err(ParseError::ParseError);
        }
        Ok(event)
    }

    pub fn into_buffer(self) -> B {
        self.0
    }
}

impl<B: AsRef<[u8]>> AsRef<[u8]> for GenericEvent<B> {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl<B: AsRef<[u8]>> Event for GenericEvent<B> {
    fn raw_bytes(&self) -> &[u8] {
        self.0.as_ref()
    }
}

const REPLY: u8 = 1;

impl<B: AsRef<[u8]>> From<GenericError<B>> for GenericEvent<B> {
    fn from(value: GenericError<B>) -> Self {
        GenericEvent(value.into_buffer())
    }
}

/// A generic error.
///
/// This struct is similar to `GenericEvent`, but is specific to error packets. It allows access to
/// the contained error code. This error code allows you to pick the right error type for
/// conversion via `TryInto::try_into()`.
#[derive(Debug, Clone)]
pub struct GenericError<B: AsRef<[u8]>>(B);

impl<B: AsRef<[u8]>> GenericError<B> {
    pub fn new(value: B) -> Result<Self, ParseError> {
        GenericEvent::new(value)?.try_into()
    }

    pub fn into_buffer(self) -> B {
        self.0
    }

    /// Get the error code of this error.
    ///
    /// The error code identifies what kind of error this packet contains. Note that extensions
    /// have their error codes dynamically assigned.
    pub fn error_code(&self) -> u8 {
        self.raw_bytes()[1]
    }
}

impl<B: AsRef<[u8]>> AsRef<[u8]> for GenericError<B> {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl<B: AsRef<[u8]>> Event for GenericError<B> {
    fn raw_bytes(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl<B: AsRef<[u8]>> TryFrom<GenericEvent<B>> for GenericError<B> {
    type Error = ParseError;

    fn try_from(event: GenericEvent<B>) -> Result<Self, Self::Error> {
        if event.response_type() != 0 {
            return Err(ParseError::ParseError);
        }
        Ok(GenericError(event.into_buffer()))
    }
}

/// A type implementing this trait can be parsed from some raw bytes.
pub trait TryParse: Sized {
    /// Try to parse the given values into an instance of this type.
    ///
    /// If parsing is successful, an instance of the type and a slice for the remaining data should
    /// be returned. Otherwise, an error is returned.
    fn try_parse(value: &[u8]) -> Result<(Self, &[u8]), ParseError>;
}

/// A type implementing this trait can be serialized into X11 raw bytes.
pub trait Serialize {
    /// The value returned by `serialize`.
    ///
    /// This should be `Vec<u8>` in most cases. However, arrays like `[u8; 4]` should also be
    /// allowed and thus this is an associated type.
    ///
    /// If generic associated types were available, implementing `AsRef<[u8]>` would be required.
    type Bytes;

    /// Serialize this value into X11 raw bytes.
    fn serialize(&self) -> Self::Bytes;

    /// Serialize this value into X11 raw bytes, appending the result into `bytes`.
    ///
    /// When calling this method, the given vector must satisfy `assert_eq!(bytes.len() % 4, 0);`.
    /// In words: Its length must be a multiple of four.
    fn serialize_into(&self, bytes: &mut Vec<u8>);
}

// Now implement TryParse and Serialize for some primitive data types that we need.

macro_rules! implement_try_parse {
    ($t:ty) => {
        impl TryParse for $t {
            fn try_parse(value: &[u8]) -> Result<(Self, &[u8]), ParseError> {
                let len = std::mem::size_of::<$t>();
                let bytes = value
                    .get(..len)
                    .ok_or(ParseError::ParseError)?
                    .try_into() // TryInto<[u8; len]>
                    .unwrap();
                Ok((<$t>::from_ne_bytes(bytes), &value[len..]))
            }
        }
    };
}

macro_rules! implement_serialize {
    ($t:ty: $size:expr) => {
        impl Serialize for $t {
            type Bytes = [u8; $size];
            fn serialize(&self) -> Self::Bytes {
                self.to_ne_bytes()
            }
            fn serialize_into(&self, bytes: &mut Vec<u8>) {
                bytes.extend_from_slice(&self.to_ne_bytes());
            }
        }
    };
}

macro_rules! forward_float {
    ($from:ty: $to:ty) => {
        impl TryParse for $from {
            fn try_parse(value: &[u8]) -> Result<(Self, &[u8]), ParseError> {
                let (data, remaining) = <$to>::try_parse(value)?;
                Ok((<$from>::from_bits(data), remaining))
            }
        }
        impl Serialize for $from {
            type Bytes = <$to as Serialize>::Bytes;
            fn serialize(&self) -> Self::Bytes {
                self.to_bits().serialize()
            }
            fn serialize_into(&self, bytes: &mut Vec<u8>) {
                self.to_bits().serialize_into(bytes);
            }
        }
    };
}

implement_try_parse!(u8);
implement_try_parse!(i8);
implement_try_parse!(u16);
implement_try_parse!(i16);
implement_try_parse!(u32);
implement_try_parse!(i32);
implement_try_parse!(u64);
implement_try_parse!(i64);

implement_serialize!(u8: 1);
implement_serialize!(i8: 1);
implement_serialize!(u16: 2);
implement_serialize!(i16: 2);
implement_serialize!(u32: 4);
implement_serialize!(i32: 4);
implement_serialize!(u64: 8);
implement_serialize!(i64: 8);

forward_float!(f32: u32);
forward_float!(f64: u64);

impl TryParse for bool {
    fn try_parse(value: &[u8]) -> Result<(Self, &[u8]), ParseError> {
        let (data, remaining) = u8::try_parse(value)?;
        Ok((data != 0, remaining))
    }
}

impl Serialize for bool {
    type Bytes = [u8; 1];
    fn serialize(&self) -> Self::Bytes {
        [u8::from(*self)]
    }
    fn serialize_into(&self, bytes: &mut Vec<u8>) {
        bytes.push(u8::from(*self));
    }
}

// Tuple handling

macro_rules! tuple_try_parse {
    ($($name:ident)*) => {
        impl<$($name,)*> TryParse for ($($name,)*)
        where $($name: TryParse,)*
        {
            #[allow(non_snake_case)]
            fn try_parse(remaining: &[u8]) -> Result<(($($name,)*), &[u8]), ParseError> {
                $(let ($name, remaining) = $name::try_parse(remaining)?;)*
                Ok((($($name,)*), remaining))
            }
        }
    }
}

macro_rules! tuple_serialize {
    ($($name:ident:$idx:tt)*) => {
        impl<$($name,)*> Serialize for ($($name,)*)
        where $($name: Serialize,)*
        {
            type Bytes = Vec<u8>;
            fn serialize(&self) -> Self::Bytes {
                let mut result = Vec::new();
                self.serialize_into(&mut result);
                result
            }
            fn serialize_into(&self, bytes: &mut Vec<u8>) {
                $(self.$idx.serialize_into(bytes);)*
            }
        }
    }
}

macro_rules! tuple_impls {
    ($($name:ident:$idx:tt)*) => {
        tuple_try_parse!($($name)*);
        tuple_serialize!($($name:$idx)*);
    }
}

// We can optimise serialisation of empty tuples or one-element-tuples with different Bytes type
impl Serialize for () {
    type Bytes = [u8; 0];
    fn serialize(&self) -> Self::Bytes {
        []
    }
    fn serialize_into(&self, _bytes: &mut Vec<u8>) {}
}

impl<T: Serialize> Serialize for (T,) {
    type Bytes = T::Bytes;
    fn serialize(&self) -> Self::Bytes {
        self.0.serialize()
    }
    fn serialize_into(&self, bytes: &mut Vec<u8>) {
        self.0.serialize_into(bytes)
    }
}

tuple_try_parse!();
tuple_try_parse!(A);
tuple_impls!(A:0 B:1);
tuple_impls!(A:0 B:1 C:2);
tuple_impls!(A:0 B:1 C:2 D:3);
tuple_impls!(A:0 B:1 C:2 D:3 E:4);
tuple_impls!(A:0 B:1 C:2 D:3 E:4 F:5);
tuple_impls!(A:0 B:1 C:2 D:3 E:4 F:5 G:6);
tuple_impls!(A:0 B:1 C:2 D:3 E:4 F:5 G:6 H:7);
tuple_impls!(A:0 B:1 C:2 D:3 E:4 F:5 G:6 H:7 I:8);
tuple_impls!(A:0 B:1 C:2 D:3 E:4 F:5 G:6 H:7 I:8 J:9);
tuple_impls!(A:0 B:1 C:2 D:3 E:4 F:5 G:6 H:7 I:8 J:9 K:10);
tuple_impls!(A:0 B:1 C:2 D:3 E:4 F:5 G:6 H:7 I:8 J:9 K:10 L:11);
tuple_impls!(A:0 B:1 C:2 D:3 E:4 F:5 G:6 H:7 I:8 J:9 K:10 L:11 M:12);
tuple_impls!(A:0 B:1 C:2 D:3 E:4 F:5 G:6 H:7 I:8 J:9 K:10 L:11 M:12 N:13);
tuple_impls!(A:0 B:1 C:2 D:3 E:4 F:5 G:6 H:7 I:8 J:9 K:10 L:11 M:12 N:13 O:14);

/// Parse a list of objects from the given data.
///
/// This function parses a list of objects where the length of the list was specified externally.
/// The wire format for `list_length` instances of `T` will be read from the given data.
pub fn parse_list<T>(data: &[u8], list_length: usize) -> Result<(Vec<T>, &[u8]), ParseError>
where
    T: TryParse,
{
    let mut remaining = data;
    let mut result = Vec::with_capacity(list_length);
    for _ in 0..list_length {
        let (entry, new_remaining) = T::try_parse(remaining)?;
        result.push(entry);
        remaining = new_remaining;
    }
    Ok((result, remaining))
}

/// Parse a list of `u8` from the given data.
pub fn parse_u8_list(data: &[u8], list_length: usize) -> Result<(&[u8], &[u8]), ParseError> {
    if data.len() < list_length {
        Err(ParseError::ParseError)
    } else {
        Ok(data.split_at(list_length))
    }
}

impl<T: Serialize> Serialize for [T] {
    type Bytes = Vec<u8>;
    fn serialize(&self) -> Self::Bytes {
        let mut result = Vec::new();
        self.serialize_into(&mut result);
        result
    }
    fn serialize_into(&self, bytes: &mut Vec<u8>) {
        for item in self {
            item.serialize_into(bytes);
        }
    }
}

// This macro is used by the generated code to implement `std::ops::BitOr` and
// `std::ops::BitOrAssign`.
macro_rules! bitmask_binop {
    ($t:ty, $u:ty) => {
        impl std::ops::BitOr for $t {
            type Output = $u;
            fn bitor(self, other: Self) -> Self::Output {
                Self::Output::from(self) | Self::Output::from(other)
            }
        }
        impl std::ops::BitOr<$u> for $t {
            type Output = $u;
            fn bitor(self, other: $u) -> Self::Output {
                Self::Output::from(self) | other
            }
        }
        impl std::ops::BitOr<$t> for $u {
            type Output = $u;
            fn bitor(self, other: $t) -> Self::Output {
                self | Self::Output::from(other)
            }
        }
        impl std::ops::BitOrAssign<$t> for $u {
            fn bitor_assign(&mut self, other: $t) {
                *self |= Self::from(other)
            }
        }
    };
}

/// A helper macro for managing atoms
///
/// If we need to use multiple atoms, one would normally write code such as
/// ```
/// # use x11rb::protocol::xproto::{Atom, ConnectionExt, InternAtomReply};
/// # use x11rb::errors::{ConnectionError, ReplyError};
/// # use x11rb::cookie::Cookie;
/// #[allow(non_snake_case)]
/// pub struct AtomCollection {
///     pub _NET_WM_NAME: Atom,
///     pub _NET_WM_ICON: Atom,
///     pub ATOM_WITH_SPACES: Atom,
///     pub WHATEVER: Atom,
/// }
///
/// #[allow(non_snake_case)]
/// struct AtomCollectionCookie<'c, C: ConnectionExt>
/// {
///     _NET_WM_NAME: Cookie<'c, C, InternAtomReply>,
///     _NET_WM_ICON: Cookie<'c, C, InternAtomReply>,
///     ATOM_WITH_SPACES: Cookie<'c, C, InternAtomReply>,
///     WHATEVER: Cookie<'c, C, InternAtomReply>,
/// }
///
/// impl AtomCollection {
///     pub fn new<C: ConnectionExt>(conn: &C) -> Result<AtomCollectionCookie<'_, C>, ConnectionError>
///     {
///         Ok(AtomCollectionCookie {
///             _NET_WM_NAME: conn.intern_atom(false, b"_NET_WM_NAME")?,
///             _NET_WM_ICON: conn.intern_atom(false, b"_NET_WM_ICON")?,
///             ATOM_WITH_SPACES: conn.intern_atom(false, b"ATOM WITH SPACES")?,
///             WHATEVER: conn.intern_atom(false, b"WHATEVER")?,
///         })
///     }
/// }
///
/// impl<'c, C> AtomCollectionCookie<'c, C>
/// where C: ConnectionExt
/// {
///     pub fn reply(self) -> Result<AtomCollection, ReplyError<C::Buf>> {
///         Ok(AtomCollection {
///             _NET_WM_NAME: self._NET_WM_NAME.reply()?.atom,
///             _NET_WM_ICON: self._NET_WM_ICON.reply()?.atom,
///             ATOM_WITH_SPACES: self.ATOM_WITH_SPACES.reply()?.atom,
///             WHATEVER: self.WHATEVER.reply()?.atom,
///         })
///     }
/// }
/// ```
/// This macro automatically produces this code with
/// ```
/// # use x11rb::atom_manager;
/// atom_manager! {
///     pub AtomCollection: AtomCollectionCookie {
///         _NET_WM_NAME,
///         _NET_WM_ICON,
///         ATOM_WITH_SPACES: b"ATOM WITH SPACES",
///         WHATEVER,
///     }
/// }
/// ```
#[macro_export]
macro_rules! atom_manager {
    {
        $vis:vis $struct_name:ident: $cookie_name:ident {
            $($field_name:ident$(: $atom_value:expr)?,)*
        }
    } => {
        // Cookie version
        #[allow(non_snake_case)]
        #[derive(Debug)]
        $vis struct $cookie_name<'a, C: $crate::protocol::xproto::ConnectionExt> {
            phantom: std::marker::PhantomData<&'a C>,
            $(
                $field_name: $crate::cookie::Cookie<'a, C, $crate::protocol::xproto::InternAtomReply>,
            )*
        }

        // Replies
        #[allow(non_snake_case)]
        #[derive(Debug, Clone, Copy)]
        $vis struct $struct_name {
            $(
                $vis $field_name: $crate::protocol::xproto::Atom,
            )*
        }

        impl $struct_name {
            $vis fn new<C: $crate::protocol::xproto::ConnectionExt>(
                _conn: &C,
            ) -> ::std::result::Result<$cookie_name<'_, C>, $crate::errors::ConnectionError> {
                Ok($cookie_name {
                    phantom: std::marker::PhantomData,
                    $(
                        $field_name: _conn.intern_atom(
                            false,
                            $crate::__atom_manager_atom_value!($field_name$(: $atom_value)?),
                        )?,
                    )*
                })
            }
        }

        impl<'a, C: $crate::protocol::xproto::ConnectionExt> $cookie_name<'a, C> {
            $vis fn reply(self) -> ::std::result::Result<$struct_name, $crate::errors::ReplyError<C::Buf>> {
                Ok($struct_name {
                    $(
                        $field_name: self.$field_name.reply()?.atom,
                    )*
                })
            }
        }
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! __atom_manager_atom_value {
    ($field_name:ident) => {
        stringify!($field_name).as_bytes()
    };
    ($field_name:ident: $atom_value:expr) => {
        $atom_value
    };
}
