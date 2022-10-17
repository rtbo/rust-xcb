mod base {
    use crate::error::{self, ProtocolError};
    use crate::event::{self, Event};
    use crate::ext::{Extension, ExtensionData};
    use crate::x::{Atom, Keysym, Setup, Timestamp};
    use crate::{cache_extensions_data, ffi::*};
    #[cfg(feature = "dl")]
    use crate::dl;
    use bitflags::bitflags;
    use libc::{c_char, c_int};
    use std::cell::RefCell;
    use std::ffi::{CStr, CString};
    use std::fmt::{self, Display, Formatter};
    use std::marker::{self, PhantomData};
    use std::mem;
    use std::os::unix::prelude::{AsRawFd, RawFd};
    use std::ptr;
    use std::result;
    use std::slice;
    /// A X resource trait
    pub trait Xid {
        /// Build a null X resource
        fn none() -> Self;
        /// Get the underlying id of the resource
        fn resource_id(&self) -> u32;
        /// Check whether this resource is null or not
        fn is_none(&self) -> bool {
            self.resource_id() == 0
        }
    }
    /// Trait for X resources that can be created directly from `Connection::generate_id`
    ///
    /// The resources that cannot be created that way are the Xid unions, which are created
    /// from their underlying resource.
    pub trait XidNew: Xid {
        /// Build a new X resource
        ///
        /// # Safety
        /// res_id must be obtained from `xcb_generate_id`. `0` is also a valid value to create a null resource.
        /// Users should not use this function directly but rather use
        /// `Connection::generate_id`
        unsafe fn new(res_id: u32) -> Self;
    }
    /// Trait for types that own a C allocated pointer and are represented by the data pointed to.
    pub trait Raw<T>: Sized {
        /// Build `Self` from a raw pointer
        ///
        /// # Safety
        /// `raw` must be a valid pointer to the representation of Self, and be allocated with `libc::malloc`
        unsafe fn from_raw(raw: *mut T) -> Self;
        /// Convert self into a raw pointer
        ///
        /// Returned value should be freed with `libc::free` or sent back to `from_raw` to avoid memory leak.
        fn into_raw(self) -> *mut T {
            let raw = self.as_raw();
            mem::forget(self);
            raw
        }
        /// Obtain the raw pointer representation
        fn as_raw(&self) -> *mut T;
    }
    /// Trait for base events (aka. non GE_GENERIC events)
    pub trait BaseEvent: Raw<xcb_generic_event_t> {
        /// The extension associated to this event, or `None` for the main protocol
        const EXTENSION: Option<Extension>;
        /// The number associated to this event
        const NUMBER: u32;
    }
    /// A trait for GE_GENERIC events
    ///
    /// A GE_GENERIC event is an extension event that does not follow
    /// the regular `response_type` offset.
    /// This system was introduced because the protocol eventually run
    /// out of event numbers.
    ///
    /// This should be completely transparent to the user, as [Event] is
    /// resolving all types of events together.
    pub trait GeEvent: Raw<xcb_ge_generic_event_t> {
        /// The extension associated to this event
        const EXTENSION: Extension;
        /// The number associated to this event
        const NUMBER: u32;
    }
    /// A trait to designate base protocol errors.
    ///
    /// The base errors follow the usual resolution idiom of `error_code` offset.
    ///
    /// This should be completely transparent to the user, as [ProtocolError] is
    /// resolving all types of errors together.
    pub trait BaseError: Raw<xcb_generic_error_t> {
        /// The extension associated to this error, or `None` for the main protocol
        const EXTENSION: Option<Extension>;
        /// The number associated to this error
        const NUMBER: u32;
    }
    /// Trait for the resolution of raw wire event to a unified event enum.
    ///
    /// `Self` is normally an enum of several event subtypes.
    /// See [crate::x::Event] and [crate::Event]
    pub(crate) trait ResolveWireEvent: Sized {
        /// Resolve a pointer to `xcb_generic_event_t` to `Self`, inferring the correct subtype
        /// using `response_type` field and `first_event`
        ///
        /// # Panics
        /// Panics if the event subtype cannot be resolved to `Self`. That is,
        /// `response_type` field must be checked beforehand to be in range with
        /// `first_event`.
        ///
        /// # Safety
        /// `event` must be a valid, non-null event returned by `xcb_wait_for_event`
        /// or similar function
        unsafe fn resolve_wire_event(
            first_event: u8,
            event: *mut xcb_generic_event_t,
        ) -> Option<Self>;
    }
    /// Trait for the resolution of raw wire GE_GENERIC event to a unified event enum.
    ///
    /// `Self` is normally an enum of several event subtypes.
    /// See [crate::xinput::Event] and [crate::Event]
    pub(crate) trait ResolveWireGeEvent: Sized {
        /// Resolve a pointer to `xcb_ge_generic_event_t` to `Self`, inferring the correct subtype
        /// using `event_type` field.
        ///
        /// # Panics
        /// Panics if the event subtype cannot be resolved for `Self`. That is, `event_type`
        /// must be checked beforehand to be in range.
        ///
        /// # Safety
        /// `event` must be a valid, non-null event returned by `xcb_wait_for_event`
        /// or similar function
        unsafe fn resolve_wire_ge_event(event: *mut xcb_ge_generic_event_t) -> Self;
    }
    /// Trait for the resolution of raw wire error to a unified error enum.
    ///
    /// `Self` is normally an enum of several event subtypes.
    /// See [crate::x::Error] and [crate::ProtocolError]
    pub(crate) trait ResolveWireError {
        /// Convert a pointer to `xcb_generic_error_t` to `Self`, inferring the correct subtype
        /// using `response_type` field and `first_error`.
        ///
        /// # Panics
        /// Panics if the error subtype cannot be resolved for `self`. That is,
        /// `response_type` field must be checked beforehand to be in range with
        /// `first_error`.
        ///
        /// # Safety
        /// `err` must be a valid, non-null error obtained by `xcb_wait_for_reply`
        /// or similar function
        unsafe fn resolve_wire_error(
            first_error: u8,
            error: *mut xcb_generic_error_t,
        ) -> Self;
    }
    /// Trait for types that can serialize themselves to the X wire.
    ///
    /// This trait is used internally for requests serialization.
    pub(crate) trait WiredOut {
        /// Compute the length of wired serialized data of self
        fn wire_len(&self) -> usize;
        /// Serialize `self` over the X wire and returns how many bytes were written.
        ///
        /// `wire_buf` must be larger or as long as the value returned by `wire_len`.
        /// `serialize` MUST write the data in its entirety at once, at the begining of `wire_buf`.
        /// That is, it returns the same value as `wire_len`.
        /// The interest in returning the value is that it is easy to compute in `serialize` and allow
        /// to easily chain serialization of fields in a struct.
        ///
        /// # Panics
        /// Panics if `wire_buf` is too small to contain the serialized representation of `self`.
        fn serialize(&self, wire_buf: &mut [u8]) -> usize;
    }
    /// Trait for types that can unserialize themselves from the X wire.
    pub(crate) trait WiredIn {
        /// type of external context necessary to figure out the representation of the data
        type Params: Copy;
        /// Compute the length of serialized data of an instance starting by `ptr`.
        ///
        /// # Safety
        /// This function is highly unsafe as the pointer must point to data that is a valid
        /// wired representation of `Self`. Failure to respect this will lead to dereferencing invalid memory.
        unsafe fn compute_wire_len(ptr: *const u8, params: Self::Params) -> usize;
        /// Unserialize an instance of `Self` from the X wire
        ///
        /// `offset` value is increased by the number of bytes corresponding to the representation of `Self`.
        /// This allows for efficient chaining of unserialization as the data offset is either known at
        /// compile time, or has to be computed anyway.
        ///
        /// # Safety
        /// This function is highly unsafe as the pointer must point to data that is a valid
        /// wired representation of `Self`. Failure to respect this will lead to dereferencing invalid memory.
        unsafe fn unserialize(
            ptr: *const u8,
            params: Self::Params,
            offset: &mut usize,
        ) -> Self;
    }
    impl WiredOut for u8 {
        fn wire_len(&self) -> usize {
            mem::size_of::<Self>()
        }
        fn serialize(&self, wire_buf: &mut [u8]) -> usize {
            if true {
                if !(wire_buf.len() >= mem::size_of::<Self>()) {
                    ::core::panicking::panic(
                        "assertion failed: wire_buf.len() >= mem::size_of::<Self>()",
                    )
                }
            }
            unsafe {
                *(wire_buf.as_mut_ptr() as *mut Self) = *self;
            }
            mem::size_of::<Self>()
        }
    }
    impl WiredIn for u8 {
        type Params = ();
        unsafe fn compute_wire_len(_ptr: *const u8, _params: Self::Params) -> usize {
            mem::size_of::<Self>()
        }
        unsafe fn unserialize(
            ptr: *const u8,
            _params: Self::Params,
            offset: &mut usize,
        ) -> Self {
            *offset += mem::size_of::<Self>();
            *(ptr as *const Self)
        }
    }
    impl WiredOut for u16 {
        fn wire_len(&self) -> usize {
            mem::size_of::<Self>()
        }
        fn serialize(&self, wire_buf: &mut [u8]) -> usize {
            if true {
                if !(wire_buf.len() >= mem::size_of::<Self>()) {
                    ::core::panicking::panic(
                        "assertion failed: wire_buf.len() >= mem::size_of::<Self>()",
                    )
                }
            }
            unsafe {
                *(wire_buf.as_mut_ptr() as *mut Self) = *self;
            }
            mem::size_of::<Self>()
        }
    }
    impl WiredIn for u16 {
        type Params = ();
        unsafe fn compute_wire_len(_ptr: *const u8, _params: Self::Params) -> usize {
            mem::size_of::<Self>()
        }
        unsafe fn unserialize(
            ptr: *const u8,
            _params: Self::Params,
            offset: &mut usize,
        ) -> Self {
            *offset += mem::size_of::<Self>();
            *(ptr as *const Self)
        }
    }
    impl WiredOut for u32 {
        fn wire_len(&self) -> usize {
            mem::size_of::<Self>()
        }
        fn serialize(&self, wire_buf: &mut [u8]) -> usize {
            if true {
                if !(wire_buf.len() >= mem::size_of::<Self>()) {
                    ::core::panicking::panic(
                        "assertion failed: wire_buf.len() >= mem::size_of::<Self>()",
                    )
                }
            }
            unsafe {
                *(wire_buf.as_mut_ptr() as *mut Self) = *self;
            }
            mem::size_of::<Self>()
        }
    }
    impl WiredIn for u32 {
        type Params = ();
        unsafe fn compute_wire_len(_ptr: *const u8, _params: Self::Params) -> usize {
            mem::size_of::<Self>()
        }
        unsafe fn unserialize(
            ptr: *const u8,
            _params: Self::Params,
            offset: &mut usize,
        ) -> Self {
            *offset += mem::size_of::<Self>();
            *(ptr as *const Self)
        }
    }
    impl WiredOut for u64 {
        fn wire_len(&self) -> usize {
            mem::size_of::<Self>()
        }
        fn serialize(&self, wire_buf: &mut [u8]) -> usize {
            if true {
                if !(wire_buf.len() >= mem::size_of::<Self>()) {
                    ::core::panicking::panic(
                        "assertion failed: wire_buf.len() >= mem::size_of::<Self>()",
                    )
                }
            }
            unsafe {
                *(wire_buf.as_mut_ptr() as *mut Self) = *self;
            }
            mem::size_of::<Self>()
        }
    }
    impl WiredIn for u64 {
        type Params = ();
        unsafe fn compute_wire_len(_ptr: *const u8, _params: Self::Params) -> usize {
            mem::size_of::<Self>()
        }
        unsafe fn unserialize(
            ptr: *const u8,
            _params: Self::Params,
            offset: &mut usize,
        ) -> Self {
            *offset += mem::size_of::<Self>();
            *(ptr as *const Self)
        }
    }
    impl WiredOut for i8 {
        fn wire_len(&self) -> usize {
            mem::size_of::<Self>()
        }
        fn serialize(&self, wire_buf: &mut [u8]) -> usize {
            if true {
                if !(wire_buf.len() >= mem::size_of::<Self>()) {
                    ::core::panicking::panic(
                        "assertion failed: wire_buf.len() >= mem::size_of::<Self>()",
                    )
                }
            }
            unsafe {
                *(wire_buf.as_mut_ptr() as *mut Self) = *self;
            }
            mem::size_of::<Self>()
        }
    }
    impl WiredIn for i8 {
        type Params = ();
        unsafe fn compute_wire_len(_ptr: *const u8, _params: Self::Params) -> usize {
            mem::size_of::<Self>()
        }
        unsafe fn unserialize(
            ptr: *const u8,
            _params: Self::Params,
            offset: &mut usize,
        ) -> Self {
            *offset += mem::size_of::<Self>();
            *(ptr as *const Self)
        }
    }
    impl WiredOut for i16 {
        fn wire_len(&self) -> usize {
            mem::size_of::<Self>()
        }
        fn serialize(&self, wire_buf: &mut [u8]) -> usize {
            if true {
                if !(wire_buf.len() >= mem::size_of::<Self>()) {
                    ::core::panicking::panic(
                        "assertion failed: wire_buf.len() >= mem::size_of::<Self>()",
                    )
                }
            }
            unsafe {
                *(wire_buf.as_mut_ptr() as *mut Self) = *self;
            }
            mem::size_of::<Self>()
        }
    }
    impl WiredIn for i16 {
        type Params = ();
        unsafe fn compute_wire_len(_ptr: *const u8, _params: Self::Params) -> usize {
            mem::size_of::<Self>()
        }
        unsafe fn unserialize(
            ptr: *const u8,
            _params: Self::Params,
            offset: &mut usize,
        ) -> Self {
            *offset += mem::size_of::<Self>();
            *(ptr as *const Self)
        }
    }
    impl WiredOut for i32 {
        fn wire_len(&self) -> usize {
            mem::size_of::<Self>()
        }
        fn serialize(&self, wire_buf: &mut [u8]) -> usize {
            if true {
                if !(wire_buf.len() >= mem::size_of::<Self>()) {
                    ::core::panicking::panic(
                        "assertion failed: wire_buf.len() >= mem::size_of::<Self>()",
                    )
                }
            }
            unsafe {
                *(wire_buf.as_mut_ptr() as *mut Self) = *self;
            }
            mem::size_of::<Self>()
        }
    }
    impl WiredIn for i32 {
        type Params = ();
        unsafe fn compute_wire_len(_ptr: *const u8, _params: Self::Params) -> usize {
            mem::size_of::<Self>()
        }
        unsafe fn unserialize(
            ptr: *const u8,
            _params: Self::Params,
            offset: &mut usize,
        ) -> Self {
            *offset += mem::size_of::<Self>();
            *(ptr as *const Self)
        }
    }
    impl WiredOut for f32 {
        fn wire_len(&self) -> usize {
            mem::size_of::<Self>()
        }
        fn serialize(&self, wire_buf: &mut [u8]) -> usize {
            if true {
                if !(wire_buf.len() >= mem::size_of::<Self>()) {
                    ::core::panicking::panic(
                        "assertion failed: wire_buf.len() >= mem::size_of::<Self>()",
                    )
                }
            }
            unsafe {
                *(wire_buf.as_mut_ptr() as *mut Self) = *self;
            }
            mem::size_of::<Self>()
        }
    }
    impl WiredIn for f32 {
        type Params = ();
        unsafe fn compute_wire_len(_ptr: *const u8, _params: Self::Params) -> usize {
            mem::size_of::<Self>()
        }
        unsafe fn unserialize(
            ptr: *const u8,
            _params: Self::Params,
            offset: &mut usize,
        ) -> Self {
            *offset += mem::size_of::<Self>();
            *(ptr as *const Self)
        }
    }
    impl<T: Xid> WiredOut for T {
        fn wire_len(&self) -> usize {
            4
        }
        fn serialize(&self, wire_buf: &mut [u8]) -> usize {
            if true {
                if !(wire_buf.len() >= 4) {
                    ::core::panicking::panic("assertion failed: wire_buf.len() >= 4")
                }
            }
            unsafe {
                *(wire_buf.as_mut_ptr() as *mut u32) = self.resource_id();
            }
            4
        }
    }
    impl<T: XidNew> WiredIn for T {
        type Params = ();
        unsafe fn compute_wire_len(_ptr: *const u8, _params: Self::Params) -> usize {
            4
        }
        unsafe fn unserialize(
            ptr: *const u8,
            _params: Self::Params,
            offset: &mut usize,
        ) -> Self {
            *offset += 4;
            let xid = *(ptr as *const u32);
            T::new(xid)
        }
    }
    /// Trait for request replies
    pub trait Reply {
        /// Build the reply struct from a raw pointer.
        ///
        /// # Safety
        /// `raw` must be a pointer to a valid wire representation of `Self`, allocated with [`libc::malloc`].
        unsafe fn from_raw(raw: *const u8) -> Self;
        /// Consume the reply struct into a raw pointer.
        ///
        /// # Safety
        /// The returned pointer must be freed with [`libc::free`] to avoid any memory leak, or be used
        /// to build another reply.
        unsafe fn into_raw(self) -> *const u8;
    }
    /// General trait for cookies returned by requests.
    pub trait Cookie {
        /// # Safety
        /// `seq` must be a valid cookie for a given `Request` or `Reply`.
        unsafe fn from_sequence(seq: u64) -> Self;
        /// The raw sequence number associated with the cookie.
        fn sequence(&self) -> u64;
    }
    /// A marker trait for a cookie that allows synchronized error checking.
    ///
    /// # Safety
    /// Cookies not implementing this trait acknowledge that the error is sent to the event loop
    ///
    /// See also [Connection::send_request], [Connection::send_request_checked] and [Connection::check_request]
    pub unsafe trait CookieChecked: Cookie {}
    /// A trait for checked cookies of requests that send a reply.
    ///
    /// # Safety
    /// Cookies implementing this trait acknowledge that their error is checked when the reply is fetched from the server.
    /// This is the default cookie type for requests with reply.
    ///
    /// See also [Connection::send_request], [Connection::wait_for_reply]
    pub unsafe trait CookieWithReplyChecked: CookieChecked {
        /// The reply type associated with the cookie
        type Reply: Reply;
    }
    /// A trait for unchecked cookies of requests that send a reply.
    ///
    /// # Safety
    /// Cookies implementing this trait acknowledge that their error is not checked when the reply is fetched from the server
    /// but in the event loop.
    ///
    /// See also [Connection::send_request_unchecked], [Connection::wait_for_event]
    pub unsafe trait CookieWithReplyUnchecked: Cookie {
        /// The reply type associated with the cookie
        type Reply: Reply;
    }
    /// The default cookie type returned by void-requests.
    ///
    /// See [Connection::send_request]
    pub struct VoidCookie {
        seq: u64,
    }
    #[automatically_derived]
    impl ::core::fmt::Debug for VoidCookie {
        fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
            ::core::fmt::Formatter::debug_struct_field1_finish(
                f,
                "VoidCookie",
                "seq",
                &&self.seq,
            )
        }
    }
    impl Cookie for VoidCookie {
        unsafe fn from_sequence(seq: u64) -> Self {
            VoidCookie { seq }
        }
        fn sequence(&self) -> u64 {
            self.seq
        }
    }
    /// The checked cookie type returned by void-requests.
    ///
    /// See [Connection::send_request_checked]
    pub struct VoidCookieChecked {
        seq: u64,
    }
    #[automatically_derived]
    impl ::core::fmt::Debug for VoidCookieChecked {
        fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
            ::core::fmt::Formatter::debug_struct_field1_finish(
                f,
                "VoidCookieChecked",
                "seq",
                &&self.seq,
            )
        }
    }
    impl Cookie for VoidCookieChecked {
        unsafe fn from_sequence(seq: u64) -> Self {
            VoidCookieChecked { seq }
        }
        fn sequence(&self) -> u64 {
            self.seq
        }
    }
    unsafe impl CookieChecked for VoidCookieChecked {}
    /// Trait implemented by all requests to send the serialized data over the wire.
    ///
    /// # Safety
    /// Types implementing this trait acknowledge that the returned value of `raw_request` correspond
    /// to a cookie for `Self` request and is checked or unchecked depending on the `checked` flag value.
    pub unsafe trait RawRequest {
        /// Actual implementation of the request sending
        ///
        /// Send the request over the `conn` wire and return a cookie sequence fitting with the `checked` flag
        /// of `Self`
        fn raw_request(&self, conn: &Connection, checked: bool) -> u64;
    }
    /// Trait implemented by requests types.
    ///
    /// See [crate::x::CreateWindow] as an example.
    pub trait Request: RawRequest {
        /// The default cookie associated to this request.
        type Cookie: Cookie;
        /// `false` if the request returns a reply, `true` otherwise.
        const IS_VOID: bool;
    }
    /// Marker trait for requests that do not return a reply.
    ///
    /// These trait is implicitely associated with [`VoidCookie`] and [`VoidCookieChecked`].
    pub trait RequestWithoutReply: Request {}
    /// Trait for requests that return a reply.
    pub trait RequestWithReply: Request {
        /// Reply associated with the request
        type Reply: Reply;
        /// Default cookie type for the request, as returned by [Connection::send_request].
        type Cookie: CookieWithReplyChecked<Reply = Self::Reply>;
        /// Unchecked cookie type for the request, as returned by [Connection::send_request_unchecked].
        type CookieUnchecked: CookieWithReplyUnchecked<Reply = Self::Reply>;
    }
    /// Container for authentication information to connect to the X server
    ///
    /// See [Connection::connect_to_display_with_auth_info] and [Connection::connect_to_fd].
    pub struct AuthInfo<'a> {
        /// String containing the authentication protocol name,
        /// such as "MIT-MAGIC-COOKIE-1" or "XDM-AUTHORIZATION-1".
        pub name: &'a str,
        /// data interpreted in a protocol specific manner
        pub data: &'a str,
    }
    #[automatically_derived]
    impl<'a> ::core::marker::Copy for AuthInfo<'a> {}
    #[automatically_derived]
    impl<'a> ::core::clone::Clone for AuthInfo<'a> {
        #[inline]
        fn clone(&self) -> AuthInfo<'a> {
            let _: ::core::clone::AssertParamIsClone<&'a str>;
            let _: ::core::clone::AssertParamIsClone<&'a str>;
            *self
        }
    }
    #[automatically_derived]
    impl<'a> ::core::fmt::Debug for AuthInfo<'a> {
        fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
            ::core::fmt::Formatter::debug_struct_field2_finish(
                f,
                "AuthInfo",
                "name",
                &&self.name,
                "data",
                &&self.data,
            )
        }
    }
    /// Display info returned by [`parse_display`]
    pub struct DisplayInfo {
        /// The hostname
        host: String,
        /// The display number
        display: i32,
        /// The screen number
        screen: i32,
    }
    #[automatically_derived]
    impl ::core::fmt::Debug for DisplayInfo {
        fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
            ::core::fmt::Formatter::debug_struct_field3_finish(
                f,
                "DisplayInfo",
                "host",
                &&self.host,
                "display",
                &&self.display,
                "screen",
                &&self.screen,
            )
        }
    }
    /// Parses a display string in the form documented by [X (7x)](https://linux.die.net/man/7/x).
    ///
    /// Returns `Some(DisplayInfo)` on success and `None` otherwise.
    /// Has no side effects on failure.
    ///
    /// If `name` empty, it uses the environment variable `DISPLAY`.
    ///
    /// If `name` does not contain a screen number, `DisplayInfo::screen` is set to `0`.
    pub fn parse_display(name: &str) -> Option<DisplayInfo> {
        unsafe {
            let name = CString::new(name).unwrap();
            let mut hostp: *mut c_char = ptr::null_mut();
            let mut display = 0i32;
            let mut screen = 0i32;
            let success = xcb_parse_display(
                name.as_ptr(),
                &mut hostp as *mut _,
                &mut display as *mut _,
                &mut screen as *mut _,
            );
            if success != 0 {
                let host = CStr::from_ptr(hostp as *const _)
                    .to_str()
                    .unwrap()
                    .to_string();
                libc::free(hostp as *mut _);
                Some(DisplayInfo {
                    host,
                    display,
                    screen,
                })
            } else {
                None
            }
        }
    }
    /// Error type that is returned by `Connection::has_error`.
    pub enum ConnError {
        /// xcb connection errors because of socket, pipe and other stream errors.
        Connection,
        /// xcb connection shutdown because of extension not supported
        ClosedExtNotSupported,
        /// malloc(), calloc() and realloc() error upon failure, for eg ENOMEM
        ClosedMemInsufficient,
        /// Connection closed, exceeding request length that server accepts.
        ClosedReqLenExceed,
        /// Connection closed, error during parsing display string.
        ClosedParseErr,
        /// Connection closed because the server does not have a screen
        /// matching the display.
        ClosedInvalidScreen,
        /// Connection closed because some file descriptor passing operation failed.
        ClosedFdPassingFailed,
    }
    #[automatically_derived]
    impl ::core::fmt::Debug for ConnError {
        fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
            match self {
                ConnError::Connection => {
                    ::core::fmt::Formatter::write_str(f, "Connection")
                }
                ConnError::ClosedExtNotSupported => {
                    ::core::fmt::Formatter::write_str(f, "ClosedExtNotSupported")
                }
                ConnError::ClosedMemInsufficient => {
                    ::core::fmt::Formatter::write_str(f, "ClosedMemInsufficient")
                }
                ConnError::ClosedReqLenExceed => {
                    ::core::fmt::Formatter::write_str(f, "ClosedReqLenExceed")
                }
                ConnError::ClosedParseErr => {
                    ::core::fmt::Formatter::write_str(f, "ClosedParseErr")
                }
                ConnError::ClosedInvalidScreen => {
                    ::core::fmt::Formatter::write_str(f, "ClosedInvalidScreen")
                }
                ConnError::ClosedFdPassingFailed => {
                    ::core::fmt::Formatter::write_str(f, "ClosedFdPassingFailed")
                }
            }
        }
    }
    impl ConnError {
        fn to_str(&self) -> &str {
            match *self {
                ConnError::Connection => "Connection error, possible I/O error",
                ConnError::ClosedExtNotSupported => {
                    "Connection closed, X extension not supported"
                }
                ConnError::ClosedMemInsufficient => {
                    "Connection closed, insufficient memory"
                }
                ConnError::ClosedReqLenExceed => {
                    "Connection closed, exceeded request length that server accepts."
                }
                ConnError::ClosedParseErr => {
                    "Connection closed, error during parsing display string"
                }
                ConnError::ClosedInvalidScreen => {
                    "Connection closed, the server does not have a screen matching the display"
                }
                ConnError::ClosedFdPassingFailed => {
                    "Connection closed, some file descriptor passing operation failed"
                }
            }
        }
    }
    impl Display for ConnError {
        fn fmt(&self, f: &mut Formatter) -> fmt::Result {
            self.to_str().fmt(f)
        }
    }
    impl std::error::Error for ConnError {
        fn description(&self) -> &str {
            self.to_str()
        }
    }
    /// The result type associated with [ConnError].
    pub type ConnResult<T> = result::Result<T, ConnError>;
    /// The result type associated with [ProtocolError].
    pub type ProtocolResult<T> = result::Result<T, ProtocolError>;
    /// The general error type for Rust-XCB.
    pub enum Error {
        /// I/O error issued from the connection.
        Connection(ConnError),
        /// A protocol related error issued by the X server.
        Protocol(ProtocolError),
        #[cfg(feature = "dl")]
        /// Dynamic library error
        DlError(dl::Error),
    }
    #[automatically_derived]
    impl ::core::fmt::Debug for Error {
        fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
            match self {
                Error::Connection(__self_0) => {
                    ::core::fmt::Formatter::debug_tuple_field1_finish(
                        f,
                        "Connection",
                        &__self_0,
                    )
                }
                Error::Protocol(__self_0) => {
                    ::core::fmt::Formatter::debug_tuple_field1_finish(
                        f,
                        "Protocol",
                        &__self_0,
                    )
                }
                Error::DlError(__self_0) => {
                    ::core::fmt::Formatter::debug_tuple_field1_finish(
                        f,
                        "DlError",
                        &__self_0,
                    )
                }
            }
        }
    }
    impl Display for Error {
        fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
            match self {
                Error::Connection(_) => f.write_str("xcb connection error"),
                Error::Protocol(_) => f.write_str("xcb protocol error"),
                #[cfg(feature = "dl")]
                Error::DlError(_) => f.write_str("dynamic library loading error"),
            }
        }
    }
    impl std::error::Error for Error {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            match self {
                Error::Connection(err) => Some(err),
                Error::Protocol(err) => Some(err),
                #[cfg(feature = "dl")]
                Error::DlError(err) => Some(err),
            }
        }
    }
    impl From<ConnError> for Error {
        fn from(err: ConnError) -> Error {
            Error::Connection(err)
        }
    }
    impl From<ProtocolError> for Error {
        fn from(err: ProtocolError) -> Error {
            Error::Protocol(err)
        }
    }
    #[cfg(feature = "dl")]
    impl From<dl::Error> for Error {
        fn from(err: dl::Error) -> Error {
            Error::DlError(err)
        }
    }
    /// The general result type for Rust-XCB.
    pub type Result<T> = result::Result<T, Error>;
    /// `Connection` is the central object of XCB.
    ///
    /// It handles all communications with the X server.
    /// It dispatches the requests, receives the replies, poll/wait the events.
    /// It also resolves the errors and events from X server.
    ///
    /// `Connection` is thread safe.
    ///
    /// It internally wraps an `xcb_connection_t` object and
    /// will call `xcb_disconnect` when the `Connection` goes out of scope.
    pub struct Connection {
        c: *mut xcb_connection_t,
        ext_data: Vec<ExtensionData>,
    }
    unsafe impl Send for Connection {}
    unsafe impl Sync for Connection {}
    impl Connection {
        /// Connects to the X server.
        ///
        /// Connects to the X server specified by `display_name.` If
        /// `display_name` is `None,` uses the value of the `DISPLAY` environment
        /// variable.
        ///
        /// If no screen is preferred, the second member of the tuple is set to 0.
        ///
        /// # Example
        /// ```no_run
        /// fn main() -> xcb::Result<()> {
        ///     let (conn, screen) = xcb::Connection::connect(None)?;
        ///     Ok(())
        /// }
        /// ```
        pub fn connect(display_name: Option<&str>) -> ConnResult<(Connection, i32)> {
            Self::connect_with_extensions(display_name, &[], &[])
        }
        /// Connects to the X server and cache extension data.
        ///
        /// Connects to the X server specified by `display_name.` If
        /// `display_name` is `None,` uses the value of the `DISPLAY` environment
        /// variable.
        ///
        /// Extension data specified by `mandatory` and `optional` is cached to allow
        /// the resolution of events and errors in these extensions.
        ///
        /// If no screen is preferred, the second member of the tuple is set to 0.
        ///
        /// # Panics
        /// Panics if one of the mandatory extension is not present.
        ///
        /// # Example
        /// ```no_run
        /// fn main() -> xcb::Result<()> {
        ///     let (conn, screen) = xcb::Connection::connect_with_extensions(
        ///         None, &[xcb::Extension::Input, xcb::Extension::Xkb], &[]
        ///     )?;
        ///     Ok(())
        /// }
        /// ```
        pub fn connect_with_extensions(
            display_name: Option<&str>,
            mandatory: &[Extension],
            optional: &[Extension],
        ) -> ConnResult<(Connection, i32)> {
            let mut screen_num: c_int = 0;
            let displayname = display_name.map(|s| CString::new(s).unwrap());
            unsafe {
                let conn = if let Some(display) = displayname {
                    xcb_connect(display.as_ptr(), &mut screen_num)
                } else {
                    xcb_connect(ptr::null(), &mut screen_num)
                };
                check_connection_error(conn)?;
                let conn = Self::from_raw_conn_and_extensions(conn, mandatory, optional);
                conn.has_error().map(|_| (conn, screen_num as i32))
            }
        }
        /// Connects to the X server with an open socket file descriptor and optional authentification info.
        ///
        /// Connects to an X server, given the open socket fd and the
        /// `auth_info`. The file descriptor `fd` is bidirectionally connected to an X server.
        /// If the connection should be unauthenticated, `auth_info` must be `None`.
        pub fn connect_to_fd(
            fd: RawFd,
            auth_info: Option<AuthInfo<'_>>,
        ) -> ConnResult<Self> {
            Self::connect_to_fd_with_extensions(fd, auth_info, &[], &[])
        }
        /// Connects to the X server with an open socket file descriptor and optional authentification info.
        ///
        /// Extension data specified by `mandatory` and `optional` is cached to allow
        /// the resolution of events and errors in these extensions.
        ///
        /// Connects to an X server, given the open socket fd and the
        /// `auth_info`. The file descriptor `fd` is bidirectionally connected to an X server.
        /// If the connection should be unauthenticated, `auth_info` must be `None`.
        ///
        /// # Panics
        /// Panics if one of the mandatory extension is not present.
        pub fn connect_to_fd_with_extensions(
            fd: RawFd,
            auth_info: Option<AuthInfo<'_>>,
            mandatory: &[Extension],
            optional: &[Extension],
        ) -> ConnResult<Self> {
            let mut auth_info = auth_info
                .map(|auth_info| {
                    let auth_name = CString::new(auth_info.name).unwrap();
                    let auth_data = CString::new(auth_info.data).unwrap();
                    let auth_info = xcb_auth_info_t {
                        namelen: auth_name.as_bytes().len() as _,
                        name: auth_name.as_ptr() as *mut _,
                        datalen: auth_data.as_bytes().len() as _,
                        data: auth_data.as_ptr() as *mut _,
                    };
                    (auth_info, auth_name, auth_data)
                });
            let ai_ptr = if let Some(auth_info) = auth_info.as_mut() {
                &mut auth_info.0 as *mut _
            } else {
                ptr::null_mut()
            };
            let conn = unsafe {
                let conn = xcb_connect_to_fd(fd, ai_ptr);
                check_connection_error(conn)?;
                Self::from_raw_conn_and_extensions(conn, mandatory, optional)
            };
            conn.has_error().map(|_| conn)
        }
        /// Connects to the X server, using an authorization information.
        ///
        /// Connects to the X server specified by `display_name`, using the
        /// authorization `auth_info`. If a particular screen on that server, it is
        /// returned in the second tuple member, which is otherwise set to `0`.
        pub fn connect_to_display_with_auth_info(
            display_name: Option<&str>,
            auth_info: AuthInfo<'_>,
        ) -> ConnResult<(Connection, i32)> {
            Self::connect_to_display_with_auth_info_and_extensions(
                display_name,
                auth_info,
                &[],
                &[],
            )
        }
        /// Connects to the X server, using an authorization information.
        ///
        /// Extension data specified by `mandatory` and `optional` is cached to allow
        /// the resolution of events and errors in these extensions.
        ///
        /// Connects to the X server specified by `display_name`, using the
        /// authorization `auth_info`. If a particular screen on that server, it is
        /// returned in the second tuple member, which is otherwise set to `0`.
        ///
        /// # Panics
        /// Panics if one of the mandatory extension is not present.
        pub fn connect_to_display_with_auth_info_and_extensions(
            display_name: Option<&str>,
            auth_info: AuthInfo<'_>,
            mandatory: &[Extension],
            optional: &[Extension],
        ) -> ConnResult<(Connection, i32)> {
            let mut screen_num: c_int = 0;
            let display_name = display_name.map(|s| CString::new(s).unwrap());
            unsafe {
                let display_name = if let Some(display_name) = &display_name {
                    display_name.as_ptr()
                } else {
                    ptr::null()
                };
                let auth_name = CString::new(auth_info.name).unwrap();
                let auth_data = CString::new(auth_info.data).unwrap();
                let mut auth_info = xcb_auth_info_t {
                    namelen: auth_name.as_bytes().len() as _,
                    name: auth_name.as_ptr() as *mut _,
                    datalen: auth_data.as_bytes().len() as _,
                    data: auth_data.as_ptr() as *mut _,
                };
                let conn = xcb_connect_to_display_with_auth_info(
                    display_name,
                    &mut auth_info as *mut _,
                    &mut screen_num as *mut _,
                );
                check_connection_error(conn)?;
                let conn = Self::from_raw_conn_and_extensions(conn, mandatory, optional);
                conn.has_error().map(|_| (conn, screen_num as i32))
            }
        }
        /// builds a new Connection object from an available connection
        ///
        /// # Safety
        /// The `conn` pointer must point to a valid `xcb_connection_t`
        pub unsafe fn from_raw_conn(conn: *mut xcb_connection_t) -> Connection {
            Self::from_raw_conn_and_extensions(conn, &[], &[])
        }
        /// Builds a new `Connection` object from an available connection and cache the extension data
        ///
        /// Extension data specified by `mandatory` and `optional` is cached to allow
        /// the resolution of events and errors in these extensions.
        ///
        /// # Panics
        /// Panics if the connection is null or in error state.
        /// Panics if one of the mandatory extension is not present.
        ///
        /// # Safety
        /// The `conn` pointer must point to a valid `xcb_connection_t`
        pub unsafe fn from_raw_conn_and_extensions(
            conn: *mut xcb_connection_t,
            mandatory: &[Extension],
            optional: &[Extension],
        ) -> Connection {
            if !!conn.is_null() {
                ::core::panicking::panic("assertion failed: !conn.is_null()")
            }
            if !check_connection_error(conn).is_ok() {
                ::core::panicking::panic(
                    "assertion failed: check_connection_error(conn).is_ok()",
                )
            }
            let ext_data = cache_extensions_data(conn, mandatory, optional);
            #[cfg(not(feature = "xlib_xcb"))] #[cfg(not(feature = "debug_atom_names"))]
            return Connection { c: conn, ext_data };
        }
        /// Get the extensions activated for this connection.
        ///
        /// You may use this to check if an optional extension is present or not.
        ///
        /// # Example
        /// ```no_run
        /// # fn main() -> xcb::Result<()> {
        ///     // Xkb is mandatory, Input is optional
        ///     let (conn, screen) = xcb::Connection::connect_with_extensions(
        ///         None, &[xcb::Extension::Xkb], &[xcb::Extension::Input]
        ///     )?;
        ///     // now we check if Input is present or not
        ///     let has_input_ext = conn.active_extensions().any(|e| e == xcb::Extension::Input);
        /// #   Ok(())
        /// # }
        /// ```
        pub fn active_extensions(&self) -> impl Iterator<Item = Extension> + '_ {
            self.ext_data.iter().map(|eed| eed.ext)
        }
        /// Returns the inner ffi `xcb_connection_t` pointer
        pub fn get_raw_conn(&self) -> *mut xcb_connection_t {
            self.c
        }
        /// Consumes this object, returning the inner ffi `xcb_connection_t` pointer
        pub fn into_raw_conn(self) -> *mut xcb_connection_t {
            let c = self.c;
            mem::forget(self);
            c
        }
        /// Returns the maximum request length that this server accepts.
        ///
        /// In the absence of the BIG-REQUESTS extension, returns the
        /// maximum request length field from the connection setup data, which
        /// may be as much as 65535. If the server supports BIG-REQUESTS, then
        /// the maximum request length field from the reply to the
        /// BigRequestsEnable request will be returned instead.
        ///
        /// Note that this length is measured in four-byte units, making the
        /// theoretical maximum lengths roughly 256kB without BIG-REQUESTS and
        /// 16GB with.
        pub fn get_maximum_request_length(&self) -> u32 {
            unsafe { xcb_get_maximum_request_length(self.c) }
        }
        /// Prefetch the maximum request length without blocking.
        ///
        /// Without blocking, does as much work as possible toward computing
        /// the maximum request length accepted by the X server.
        ///
        /// Invoking this function may send the [crate::bigreq::Enable] request,
        /// but will not block waiting for the reply.
        /// [Connection::get_maximum_request_length] will return the prefetched data
        /// after possibly blocking while the reply is retrieved.
        ///
        /// Note that in order for this function to be fully non-blocking, the
        /// application must previously have called [crate::bigreq::prefetch_extension_data].
        pub fn prefetch_maximum_request_length(&self) {
            unsafe {
                xcb_prefetch_maximum_request_length(self.c);
            }
        }
        /// Allocates an XID for a new object.
        ///
        /// Returned value is typically used in requests such as `CreateWindow`.
        ///
        /// # Example
        /// ```no_run
        /// # use xcb::x;
        /// # fn main() -> xcb::Result<()> {
        /// # let conn = xcb::Connection::connect(None)?.0;
        /// let window: x::Window = conn.generate_id();
        /// # Ok(())
        /// # }
        /// ```
        pub fn generate_id<T: XidNew>(&self) -> T {
            unsafe { XidNew::new(xcb_generate_id(self.c)) }
        }
        /// Forces any buffered output to be written to the server.
        ///
        /// Forces any buffered output to be written to the server. Blocks
        /// until the write is complete.
        ///
        /// There are several occasions ones want to flush the connection.
        /// One of them is before entering or re-entering the event loop after performing unchecked requests.
        ///
        /// The main difference between `flush` and `check_request` is that `flush` will not report protocol errors.
        /// If a protocol error is emitted by an unchecked void request, it will be reported through the event loop.
        ///
        /// See also: [wait_for_event](Connection::wait_for_event), [check_request](Connection::check_request),
        /// [send_and_check_request](Connection::send_and_check_request).
        pub fn flush(&self) -> ConnResult<()> {
            unsafe {
                let ret = xcb_flush(self.c);
                if ret > 0 {
                    Ok(())
                } else {
                    self.has_error()?;
                    ::core::panicking::panic("internal error: entered unreachable code")
                }
            }
        }
        unsafe fn handle_wait_for_event(
            &self,
            ev: *mut xcb_generic_event_t,
        ) -> Result<Event> {
            if ev.is_null() {
                self.has_error()?;
                {
                    ::std::rt::begin_panic(
                        "xcb_wait_for_event returned null with I/O error",
                    )
                };
            } else if is_error(ev) {
                Err(error::resolve_error(ev as *mut _, &self.ext_data).into())
            } else {
                Ok(event::resolve_event(ev, &self.ext_data))
            }
        }
        /// Resolve an xcb_generic_event_t pointer into an Event.
        /// # Safety
        /// The caller is repsonsible to ensure that the `ev` pointer is not NULL.
        /// The ownership of the pointer is effectively transferred to the
        /// returned Event and it will be destroyed when the Event is
        /// dropped.
        pub unsafe fn resolve_event(&self, ev: &mut xcb_generic_event_t) -> Event {
            event::resolve_event(ev, &self.ext_data)
        }
        unsafe fn handle_poll_for_event(
            &self,
            ev: *mut xcb_generic_event_t,
        ) -> Result<Option<Event>> {
            if ev.is_null() {
                self.has_error()?;
                Ok(None)
            } else if is_error(ev) {
                Err(error::resolve_error(ev as *mut _, &self.ext_data).into())
            } else {
                Ok(Some(event::resolve_event(ev, &self.ext_data)))
            }
        }
        /// Blocks and returns the next event or error from the server.
        ///
        /// # Example
        /// ```no_run
        /// use xcb::x;
        /// fn main() -> xcb::Result<()> {
        /// #   let conn = xcb::Connection::connect(None)?.0;
        ///     // ...
        ///     loop {
        ///         let event = match conn.wait_for_event() {
        ///             Err(xcb::Error::Connection(err)) => {
        ///                 panic!("unexpected I/O error: {}", err);
        ///             }
        ///             Err(xcb::Error::Protocol(xcb::ProtocolError::X(x::Error::Font(err), _req_name))) => {
        ///                 // may be this particular error is fine?
        ///                 continue;
        ///             }
        ///             Err(xcb::Error::Protocol(err)) => {
        ///                 panic!("unexpected protocol error: {:#?}", err);
        ///             }
        ///             Ok(event) => event,
        ///         };
        ///         match event {
        ///             xcb::Event::X(x::Event::KeyPress(ev)) => {
        ///                 // do stuff with the key press
        ///             }
        ///             // handle other events
        ///             _ => {
        ///                 break Ok(());
        ///             }
        ///         }
        ///     }
        ///  }
        /// ```
        pub fn wait_for_event(&self) -> Result<Event> {
            unsafe {
                let ev = xcb_wait_for_event(self.c);
                self.handle_wait_for_event(ev)
            }
        }
        /// Returns the next event or error from the server without blocking.
        ///
        /// Returns the next event or error from the server, if one is
        /// available. If no event is available, that
        /// might be because an I/O error like connection close occurred while
        /// attempting to read the next event, in which case the connection is
        /// shut down when this function returns.
        pub fn poll_for_event(&self) -> Result<Option<Event>> {
            unsafe {
                let ev = xcb_poll_for_event(self.c);
                self.handle_poll_for_event(ev)
            }
        }
        /// Returns the next event without reading from the connection.
        ///
        /// This is a version of [Connection::poll_for_event] that only examines the
        /// event queue for new events. The function doesn't try to read new
        /// events from the connection if no queued events are found.
        ///
        /// This function is useful for callers that know in advance that all
        /// interesting events have already been read from the connection. For
        /// example, callers might use [Connection::wait_for_reply] and be interested
        /// only of events that preceded a specific reply.
        pub fn poll_for_queued_event(&self) -> ProtocolResult<Option<Event>> {
            unsafe {
                let ev = xcb_poll_for_queued_event(self.c);
                if ev.is_null() {
                    Ok(None)
                } else if is_error(ev) {
                    Err(error::resolve_error(ev as *mut _, &self.ext_data))
                } else {
                    Ok(Some(event::resolve_event(ev, &self.ext_data)))
                }
            }
        }
        /// Discards the reply for a request.
        ///
        /// Discards the reply for a request. Additionally, any error generated
        /// by the request is also discarded (unless it was an _unchecked request
        /// and the error has already arrived).
        ///
        /// This function will not block even if the reply is not yet available.
        fn discard_reply<C: Cookie>(&self, cookie: C) {
            unsafe {
                xcb_discard_reply64(self.c, cookie.sequence());
            }
        }
        /// Access the data returned by the server.
        ///
        /// Accessor for the data returned by the server when the connection
        /// was initialized. This data includes
        /// - the server's required format for images,
        /// - a list of available visuals,
        /// - a list of available screens,
        /// - the server's maximum request length (in the absence of the
        /// BIG-REQUESTS extension),
        /// - and other assorted information.
        ///
        /// See the X protocol specification for more details.
        pub fn get_setup(&self) -> &Setup {
            unsafe {
                let ptr = xcb_get_setup(self.c);
                let mut _offset = 0;
                <&Setup as WiredIn>::unserialize(ptr, (), &mut _offset)
            }
        }
        /// Test whether the connection has shut down due to a fatal error.
        ///
        /// Some errors that occur in the context of a connection
        /// are unrecoverable. When such an error occurs, the
        /// connection is shut down and further operations on the
        /// connection have no effect.
        pub fn has_error(&self) -> ConnResult<()> {
            unsafe { check_connection_error(self.c) }
        }
        /// Send a request to the X server.
        ///
        /// This function never blocks. A cookie is returned to keep track of the request.
        /// If the request expect a reply, the cookie can be used to retrieve the reply with
        /// [Connection::wait_for_reply].
        ///
        /// # Example
        /// ```no_run
        /// # use xcb::x;
        /// # fn main() -> xcb::Result<()> {
        /// #   let (conn, screen_num) = xcb::Connection::connect(None)?;
        /// #   let setup = conn.get_setup();
        /// #   let screen = setup.roots().nth(screen_num as usize).unwrap();
        /// #   let window: x::Window = conn.generate_id();
        ///     // Example of void request.
        ///     // Error (if any) will be sent to the event loop (see `wait_for_event`).
        ///     // In this case, the cookie can be discarded.
        ///     conn.send_request(&x::CreateWindow {
        ///         depth: x::COPY_FROM_PARENT as u8,
        ///         wid: window,
        ///         parent: screen.root(),
        ///         x: 0,
        ///         y: 0,
        ///         width: 150,
        ///         height: 150,
        ///         border_width: 10,
        ///         class: x::WindowClass::InputOutput,
        ///         visual: screen.root_visual(),
        ///         value_list: &[
        ///             x::Cw::BackPixel(screen.white_pixel()),
        ///             x::Cw::EventMask(x::EventMask::EXPOSURE | x::EventMask::KEY_PRESS),
        ///         ],
        ///     });
        ///
        ///     // Example of request with reply. The error (if any) is obtained with the reply.
        ///     let cookie = conn.send_request(&x::InternAtom {
        ///         only_if_exists: true,
        ///         name: b"WM_PROTOCOLS",
        ///     });
        ///     let wm_protocols_atom: x::Atom = conn
        ///             .wait_for_reply(cookie)?
        ///             .atom();
        /// #   Ok(())
        /// # }
        /// ```
        pub fn send_request<R>(&self, req: &R) -> R::Cookie
        where
            R: Request,
        {
            unsafe { R::Cookie::from_sequence(req.raw_request(self, !R::IS_VOID)) }
        }
        /// Send a checked request to the X server.
        ///
        /// Checked requests do not expect a reply, but the returned cookie can be used to check for
        /// errors using `Connection::check_request`.
        ///
        /// # Example
        /// ```no_run
        /// # use xcb::x;
        /// # fn main() -> xcb::Result<()> {
        /// #   let (conn, screen_num) = xcb::Connection::connect(None)?;
        /// #   let window: x::Window = conn.generate_id();
        ///     let cookie = conn.send_request_checked(&x::MapWindow { window });
        ///     conn.check_request(cookie)?;
        /// #   Ok(())
        /// # }
        /// ```
        pub fn send_request_checked<R>(&self, req: &R) -> VoidCookieChecked
        where
            R: RequestWithoutReply,
        {
            unsafe { VoidCookieChecked::from_sequence(req.raw_request(self, true)) }
        }
        /// Send an unchecked request to the X server.
        ///
        /// Unchecked requests expect a reply that is to be retrieved by [Connection::wait_for_reply_unchecked].
        /// Unchecked means that the error is not checked when the reply is fetched. Instead, the error will
        /// be sent to the event loop
        ///
        /// # Example
        /// ```no_run
        /// # use xcb::x;
        /// # fn main() -> xcb::Result<()> {
        /// #   let (conn, screen_num) = xcb::Connection::connect(None)?;
        ///     let cookie = conn.send_request_unchecked(&x::InternAtom {
        ///         only_if_exists: true,
        ///         name: b"WM_PROTOCOLS",
        ///     });
        ///     let wm_protocols_atom: Option<x::Atom> = conn
        ///             .wait_for_reply_unchecked(cookie)?
        ///             .map(|rep| rep.atom());
        /// #   Ok(())
        /// # }
        /// ```
        pub fn send_request_unchecked<R>(&self, req: &R) -> R::CookieUnchecked
        where
            R: RequestWithReply,
        {
            unsafe { R::CookieUnchecked::from_sequence(req.raw_request(self, false)) }
        }
        /// Check a checked request for errors.
        ///
        /// The cookie supplied to this function must have resulted
        /// from a call to [Connection::send_request_checked].  This function will block
        /// until one of two conditions happens.  If an error is received, it will be
        /// returned.  If a reply to a subsequent request has already arrived, no error
        /// can arrive for this request, so this function will return `Ok(())`.
        ///
        /// Note that this function will perform a sync if needed to ensure that the
        /// sequence number will advance beyond that provided in cookie; this is a
        /// convenience to avoid races in determining whether the sync is needed.
        ///
        /// # Example
        /// ```no_run
        /// # use xcb::x;
        /// # fn main() -> xcb::Result<()> {
        /// #   let (conn, screen_num) = xcb::Connection::connect(None)?;
        /// #   let window: x::Window = conn.generate_id();
        ///     conn.check_request(conn.send_request_checked(&x::MapWindow { window }))?;
        /// #   Ok(())
        /// # }
        /// ```
        pub fn check_request(&self, cookie: VoidCookieChecked) -> ProtocolResult<()> {
            let cookie = xcb_void_cookie_t {
                seq: cookie.sequence() as u32,
            };
            let error = unsafe { xcb_request_check(self.c, cookie) };
            if error.is_null() {
                Ok(())
            } else {
                unsafe {
                    let res = error::resolve_error(error, &self.ext_data);
                    Err(res)
                }
            }
        }
        /// Send the request to the server and check it.
        ///
        /// This is a sugar for `conn.check_request(conn.send_request_checked(req))`
        ///
        /// This method is useful as well in place of code sending a void request
        /// and flushing the connection right after. Checking the request effectively
        /// flushes the connection, but in addition reports possible protocol errors
        /// at the calling site instead of reporting them through the event loop.
        ///
        /// # Example
        /// ```no_run
        /// # use xcb::x;
        /// # fn main() -> xcb::Result<()> {
        /// #   let (conn, screen_num) = xcb::Connection::connect(None)?;
        /// #   let window: x::Window = conn.generate_id();
        ///     conn.send_and_check_request(&x::MapWindow { window })?;
        /// #   Ok(())
        /// # }
        /// ```
        pub fn send_and_check_request<R>(&self, req: &R) -> ProtocolResult<()>
        where
            R: RequestWithoutReply,
        {
            self.check_request(self.send_request_checked(req))
        }
        /// Get the reply of a previous request, or an error if one occured.
        ///
        /// # Example
        /// ```no_run
        /// # use xcb::x;
        /// # fn main() -> xcb::Result<()> {
        /// #   let (conn, screen_num) = xcb::Connection::connect(None)?;
        ///     let cookie = conn.send_request(&x::InternAtom {
        ///         only_if_exists: true,
        ///         name: b"WM_PROTOCOLS",
        ///     });
        ///     let wm_protocols_atom: x::Atom = conn
        ///             .wait_for_reply(cookie)?
        ///             .atom();
        /// #   Ok(())
        /// # }
        /// ```
        pub fn wait_for_reply<C>(&self, cookie: C) -> Result<C::Reply>
        where
            C: CookieWithReplyChecked,
        {
            unsafe {
                let mut error: *mut xcb_generic_error_t = std::ptr::null_mut();
                let reply = xcb_wait_for_reply64(
                    self.c,
                    cookie.sequence(),
                    &mut error as *mut _,
                );
                match (reply.is_null(), error.is_null()) {
                    (true, true) => {
                        self.has_error()?;
                        ::core::panicking::unreachable_display(
                            &"xcb_wait_for_reply64 returned null without I/O error",
                        );
                    }
                    (true, false) => {
                        let error = error::resolve_error(error, &self.ext_data);
                        Err(error.into())
                    }
                    (false, true) => Ok(C::Reply::from_raw(reply as *const u8)),
                    (false, false) => {
                        ::core::panicking::unreachable_display(
                            &"xcb_wait_for_reply64 returned two pointers",
                        )
                    }
                }
            }
        }
        /// Get the reply of a previous unchecked request.
        ///
        /// If an error occured, `None` is returned and the error will be delivered to the event loop.
        ///
        /// # Example
        /// ```no_run
        /// # use xcb::x;
        /// # fn main() -> xcb::Result<()> {
        /// #   let (conn, screen_num) = xcb::Connection::connect(None)?;
        ///     let cookie = conn.send_request_unchecked(&x::InternAtom {
        ///         only_if_exists: true,
        ///         name: b"WM_PROTOCOLS",
        ///     });
        ///     let wm_protocols_atom: Option<x::Atom> = conn
        ///             .wait_for_reply_unchecked(cookie)?  // connection error may happen
        ///             .map(|rep| rep.atom());
        /// #   Ok(())
        /// # }
        /// ```
        pub fn wait_for_reply_unchecked<C>(
            &self,
            cookie: C,
        ) -> ConnResult<Option<C::Reply>>
        where
            C: CookieWithReplyUnchecked,
        {
            unsafe {
                let reply = xcb_wait_for_reply64(
                    self.c,
                    cookie.sequence(),
                    ptr::null_mut(),
                );
                if reply.is_null() {
                    self.has_error()?;
                    Ok(None)
                } else {
                    Ok(Some(C::Reply::from_raw(reply as *const u8)))
                }
            }
        }
        /// Obtain number of bytes read from the connection.
        ///
        /// Returns cumulative number of bytes received from the connection.
        ///
        /// This retrieves the total number of bytes read from this connection,
        /// to be used for diagnostic/monitoring/informative purposes.
        pub fn total_read(&self) -> usize {
            unsafe { xcb_total_read(self.c) as usize }
        }
        /// Obtain number of bytes written to the connection.
        ///
        /// Returns cumulative number of bytes sent to the connection.
        ///
        /// This retrieves the total number of bytes written to this connection,
        /// to be used for diagnostic/monitoring/informative purposes.
        pub fn total_written(&self) -> usize {
            unsafe { xcb_total_written(self.c) as usize }
        }
    }
    #[cfg(feature = "dl")]
    fn open_xcblib() -> ConnResult<XcbLib> {
        Ok(XcbLib::open()?)
    }
    impl AsRef<Connection> for Connection {
        fn as_ref(&self) -> &Connection {
            self
        }
    }
    impl AsRawFd for Connection {
        fn as_raw_fd(&self) -> RawFd {
            unsafe { xcb_get_file_descriptor(self.c) }
        }
    }
    impl Drop for Connection {
        fn drop(&mut self) {
            #[cfg(not(feature = "xlib_xcb"))]
            unsafe {
                xcb_disconnect(self.c);
            }
        }
    }
    unsafe fn check_connection_error(conn: *mut xcb_connection_t) -> ConnResult<()> {
        match xcb_connection_has_error(conn) {
            0 => Ok(()),
            XCB_CONN_ERROR => Err(ConnError::Connection),
            XCB_CONN_CLOSED_EXT_NOTSUPPORTED => Err(ConnError::ClosedExtNotSupported),
            XCB_CONN_CLOSED_MEM_INSUFFICIENT => Err(ConnError::ClosedMemInsufficient),
            XCB_CONN_CLOSED_REQ_LEN_EXCEED => Err(ConnError::ClosedReqLenExceed),
            XCB_CONN_CLOSED_PARSE_ERR => Err(ConnError::ClosedParseErr),
            XCB_CONN_CLOSED_INVALID_SCREEN => Err(ConnError::ClosedInvalidScreen),
            XCB_CONN_CLOSED_FDPASSING_FAILED => Err(ConnError::ClosedFdPassingFailed),
            code => {
                ::core::panicking::panic_fmt(
                    ::core::fmt::Arguments::new_v1(
                        &[
                            "internal error: entered unreachable code: unexpected error code from XCB: ",
                        ],
                        &[::core::fmt::ArgumentV1::new_display(&code)],
                    ),
                )
            }
        }
    }
    unsafe fn is_error(ev: *mut xcb_generic_event_t) -> bool {
        if true {
            if !!ev.is_null() {
                ::core::panicking::panic("assertion failed: !ev.is_null()")
            }
        }
        (*ev).response_type == 0
    }
    pub(crate) struct RequestFlags {
        bits: u32,
    }
    #[automatically_derived]
    impl ::core::marker::Copy for RequestFlags {}
    impl ::core::marker::StructuralPartialEq for RequestFlags {}
    #[automatically_derived]
    impl ::core::cmp::PartialEq for RequestFlags {
        #[inline]
        fn eq(&self, other: &RequestFlags) -> bool {
            self.bits == other.bits
        }
    }
    impl ::core::marker::StructuralEq for RequestFlags {}
    #[automatically_derived]
    impl ::core::cmp::Eq for RequestFlags {
        #[inline]
        #[doc(hidden)]
        #[no_coverage]
        fn assert_receiver_is_total_eq(&self) -> () {
            let _: ::core::cmp::AssertParamIsEq<u32>;
        }
    }
    #[automatically_derived]
    impl ::core::clone::Clone for RequestFlags {
        #[inline]
        fn clone(&self) -> RequestFlags {
            let _: ::core::clone::AssertParamIsClone<u32>;
            *self
        }
    }
    #[automatically_derived]
    impl ::core::cmp::PartialOrd for RequestFlags {
        #[inline]
        fn partial_cmp(
            &self,
            other: &RequestFlags,
        ) -> ::core::option::Option<::core::cmp::Ordering> {
            ::core::cmp::PartialOrd::partial_cmp(&self.bits, &other.bits)
        }
    }
    #[automatically_derived]
    impl ::core::cmp::Ord for RequestFlags {
        #[inline]
        fn cmp(&self, other: &RequestFlags) -> ::core::cmp::Ordering {
            ::core::cmp::Ord::cmp(&self.bits, &other.bits)
        }
    }
    #[automatically_derived]
    impl ::core::hash::Hash for RequestFlags {
        fn hash<__H: ::core::hash::Hasher>(&self, state: &mut __H) -> () {
            ::core::hash::Hash::hash(&self.bits, state)
        }
    }
    impl ::bitflags::_core::fmt::Debug for RequestFlags {
        fn fmt(
            &self,
            f: &mut ::bitflags::_core::fmt::Formatter,
        ) -> ::bitflags::_core::fmt::Result {
            #[allow(non_snake_case)]
            trait __BitFlags {
                #[inline]
                fn NONE(&self) -> bool {
                    false
                }
                #[inline]
                fn CHECKED(&self) -> bool {
                    false
                }
                #[inline]
                fn RAW(&self) -> bool {
                    false
                }
                #[inline]
                fn DISCARD_REPLY(&self) -> bool {
                    false
                }
                #[inline]
                fn REPLY_FDS(&self) -> bool {
                    false
                }
            }
            #[allow(non_snake_case)]
            impl __BitFlags for RequestFlags {
                #[allow(deprecated)]
                #[inline]
                fn NONE(&self) -> bool {
                    if Self::NONE.bits == 0 && self.bits != 0 {
                        false
                    } else {
                        self.bits & Self::NONE.bits == Self::NONE.bits
                    }
                }
                #[allow(deprecated)]
                #[inline]
                fn CHECKED(&self) -> bool {
                    if Self::CHECKED.bits == 0 && self.bits != 0 {
                        false
                    } else {
                        self.bits & Self::CHECKED.bits == Self::CHECKED.bits
                    }
                }
                #[allow(deprecated)]
                #[inline]
                fn RAW(&self) -> bool {
                    if Self::RAW.bits == 0 && self.bits != 0 {
                        false
                    } else {
                        self.bits & Self::RAW.bits == Self::RAW.bits
                    }
                }
                #[allow(deprecated)]
                #[inline]
                fn DISCARD_REPLY(&self) -> bool {
                    if Self::DISCARD_REPLY.bits == 0 && self.bits != 0 {
                        false
                    } else {
                        self.bits & Self::DISCARD_REPLY.bits == Self::DISCARD_REPLY.bits
                    }
                }
                #[allow(deprecated)]
                #[inline]
                fn REPLY_FDS(&self) -> bool {
                    if Self::REPLY_FDS.bits == 0 && self.bits != 0 {
                        false
                    } else {
                        self.bits & Self::REPLY_FDS.bits == Self::REPLY_FDS.bits
                    }
                }
            }
            let mut first = true;
            if <Self as __BitFlags>::NONE(self) {
                if !first {
                    f.write_str(" | ")?;
                }
                first = false;
                f.write_str("NONE")?;
            }
            if <Self as __BitFlags>::CHECKED(self) {
                if !first {
                    f.write_str(" | ")?;
                }
                first = false;
                f.write_str("CHECKED")?;
            }
            if <Self as __BitFlags>::RAW(self) {
                if !first {
                    f.write_str(" | ")?;
                }
                first = false;
                f.write_str("RAW")?;
            }
            if <Self as __BitFlags>::DISCARD_REPLY(self) {
                if !first {
                    f.write_str(" | ")?;
                }
                first = false;
                f.write_str("DISCARD_REPLY")?;
            }
            if <Self as __BitFlags>::REPLY_FDS(self) {
                if !first {
                    f.write_str(" | ")?;
                }
                first = false;
                f.write_str("REPLY_FDS")?;
            }
            let extra_bits = self.bits & !Self::all().bits();
            if extra_bits != 0 {
                if !first {
                    f.write_str(" | ")?;
                }
                first = false;
                f.write_str("0x")?;
                ::bitflags::_core::fmt::LowerHex::fmt(&extra_bits, f)?;
            }
            if first {
                f.write_str("(empty)")?;
            }
            Ok(())
        }
    }
    impl ::bitflags::_core::fmt::Binary for RequestFlags {
        fn fmt(
            &self,
            f: &mut ::bitflags::_core::fmt::Formatter,
        ) -> ::bitflags::_core::fmt::Result {
            ::bitflags::_core::fmt::Binary::fmt(&self.bits, f)
        }
    }
    impl ::bitflags::_core::fmt::Octal for RequestFlags {
        fn fmt(
            &self,
            f: &mut ::bitflags::_core::fmt::Formatter,
        ) -> ::bitflags::_core::fmt::Result {
            ::bitflags::_core::fmt::Octal::fmt(&self.bits, f)
        }
    }
    impl ::bitflags::_core::fmt::LowerHex for RequestFlags {
        fn fmt(
            &self,
            f: &mut ::bitflags::_core::fmt::Formatter,
        ) -> ::bitflags::_core::fmt::Result {
            ::bitflags::_core::fmt::LowerHex::fmt(&self.bits, f)
        }
    }
    impl ::bitflags::_core::fmt::UpperHex for RequestFlags {
        fn fmt(
            &self,
            f: &mut ::bitflags::_core::fmt::Formatter,
        ) -> ::bitflags::_core::fmt::Result {
            ::bitflags::_core::fmt::UpperHex::fmt(&self.bits, f)
        }
    }
    #[allow(dead_code)]
    impl RequestFlags {
        pub const NONE: Self = Self { bits: 0 };
        pub const CHECKED: Self = Self { bits: 1 };
        pub const RAW: Self = Self { bits: 2 };
        pub const DISCARD_REPLY: Self = Self { bits: 4 };
        pub const REPLY_FDS: Self = Self { bits: 8 };
        /// Returns an empty set of flags.
        #[inline]
        pub const fn empty() -> Self {
            Self { bits: 0 }
        }
        /// Returns the set containing all flags.
        #[inline]
        pub const fn all() -> Self {
            #[allow(non_snake_case)]
            trait __BitFlags {
                const NONE: u32 = 0;
                const CHECKED: u32 = 0;
                const RAW: u32 = 0;
                const DISCARD_REPLY: u32 = 0;
                const REPLY_FDS: u32 = 0;
            }
            #[allow(non_snake_case)]
            impl __BitFlags for RequestFlags {
                #[allow(deprecated)]
                const NONE: u32 = Self::NONE.bits;
                #[allow(deprecated)]
                const CHECKED: u32 = Self::CHECKED.bits;
                #[allow(deprecated)]
                const RAW: u32 = Self::RAW.bits;
                #[allow(deprecated)]
                const DISCARD_REPLY: u32 = Self::DISCARD_REPLY.bits;
                #[allow(deprecated)]
                const REPLY_FDS: u32 = Self::REPLY_FDS.bits;
            }
            Self {
                bits: <Self as __BitFlags>::NONE | <Self as __BitFlags>::CHECKED
                    | <Self as __BitFlags>::RAW | <Self as __BitFlags>::DISCARD_REPLY
                    | <Self as __BitFlags>::REPLY_FDS,
            }
        }
        /// Returns the raw value of the flags currently stored.
        #[inline]
        pub const fn bits(&self) -> u32 {
            self.bits
        }
        /// Convert from underlying bit representation, unless that
        /// representation contains bits that do not correspond to a flag.
        #[inline]
        pub const fn from_bits(bits: u32) -> ::bitflags::_core::option::Option<Self> {
            if (bits & !Self::all().bits()) == 0 {
                ::bitflags::_core::option::Option::Some(Self { bits })
            } else {
                ::bitflags::_core::option::Option::None
            }
        }
        /// Convert from underlying bit representation, dropping any bits
        /// that do not correspond to flags.
        #[inline]
        pub const fn from_bits_truncate(bits: u32) -> Self {
            Self {
                bits: bits & Self::all().bits,
            }
        }
        /// Convert from underlying bit representation, preserving all
        /// bits (even those not corresponding to a defined flag).
        ///
        /// # Safety
        ///
        /// The caller of the `bitflags!` macro can chose to allow or
        /// disallow extra bits for their bitflags type.
        ///
        /// The caller of `from_bits_unchecked()` has to ensure that
        /// all bits correspond to a defined flag or that extra bits
        /// are valid for this bitflags type.
        #[inline]
        pub const unsafe fn from_bits_unchecked(bits: u32) -> Self {
            Self { bits }
        }
        /// Returns `true` if no flags are currently stored.
        #[inline]
        pub const fn is_empty(&self) -> bool {
            self.bits() == Self::empty().bits()
        }
        /// Returns `true` if all flags are currently set.
        #[inline]
        pub const fn is_all(&self) -> bool {
            Self::all().bits | self.bits == self.bits
        }
        /// Returns `true` if there are flags common to both `self` and `other`.
        #[inline]
        pub const fn intersects(&self, other: Self) -> bool {
            !(Self {
                bits: self.bits & other.bits,
            })
                .is_empty()
        }
        /// Returns `true` if all of the flags in `other` are contained within `self`.
        #[inline]
        pub const fn contains(&self, other: Self) -> bool {
            (self.bits & other.bits) == other.bits
        }
        /// Inserts the specified flags in-place.
        #[inline]
        pub fn insert(&mut self, other: Self) {
            self.bits |= other.bits;
        }
        /// Removes the specified flags in-place.
        #[inline]
        pub fn remove(&mut self, other: Self) {
            self.bits &= !other.bits;
        }
        /// Toggles the specified flags in-place.
        #[inline]
        pub fn toggle(&mut self, other: Self) {
            self.bits ^= other.bits;
        }
        /// Inserts or removes the specified flags depending on the passed value.
        #[inline]
        pub fn set(&mut self, other: Self, value: bool) {
            if value {
                self.insert(other);
            } else {
                self.remove(other);
            }
        }
        /// Returns the intersection between the flags in `self` and
        /// `other`.
        ///
        /// Specifically, the returned set contains only the flags which are
        /// present in *both* `self` *and* `other`.
        ///
        /// This is equivalent to using the `&` operator (e.g.
        /// [`ops::BitAnd`]), as in `flags & other`.
        ///
        /// [`ops::BitAnd`]: https://doc.rust-lang.org/std/ops/trait.BitAnd.html
        #[inline]
        #[must_use]
        pub const fn intersection(self, other: Self) -> Self {
            Self {
                bits: self.bits & other.bits,
            }
        }
        /// Returns the union of between the flags in `self` and `other`.
        ///
        /// Specifically, the returned set contains all flags which are
        /// present in *either* `self` *or* `other`, including any which are
        /// present in both (see [`Self::symmetric_difference`] if that
        /// is undesirable).
        ///
        /// This is equivalent to using the `|` operator (e.g.
        /// [`ops::BitOr`]), as in `flags | other`.
        ///
        /// [`ops::BitOr`]: https://doc.rust-lang.org/std/ops/trait.BitOr.html
        #[inline]
        #[must_use]
        pub const fn union(self, other: Self) -> Self {
            Self {
                bits: self.bits | other.bits,
            }
        }
        /// Returns the difference between the flags in `self` and `other`.
        ///
        /// Specifically, the returned set contains all flags present in
        /// `self`, except for the ones present in `other`.
        ///
        /// It is also conceptually equivalent to the "bit-clear" operation:
        /// `flags & !other` (and this syntax is also supported).
        ///
        /// This is equivalent to using the `-` operator (e.g.
        /// [`ops::Sub`]), as in `flags - other`.
        ///
        /// [`ops::Sub`]: https://doc.rust-lang.org/std/ops/trait.Sub.html
        #[inline]
        #[must_use]
        pub const fn difference(self, other: Self) -> Self {
            Self {
                bits: self.bits & !other.bits,
            }
        }
        /// Returns the [symmetric difference][sym-diff] between the flags
        /// in `self` and `other`.
        ///
        /// Specifically, the returned set contains the flags present which
        /// are present in `self` or `other`, but that are not present in
        /// both. Equivalently, it contains the flags present in *exactly
        /// one* of the sets `self` and `other`.
        ///
        /// This is equivalent to using the `^` operator (e.g.
        /// [`ops::BitXor`]), as in `flags ^ other`.
        ///
        /// [sym-diff]: https://en.wikipedia.org/wiki/Symmetric_difference
        /// [`ops::BitXor`]: https://doc.rust-lang.org/std/ops/trait.BitXor.html
        #[inline]
        #[must_use]
        pub const fn symmetric_difference(self, other: Self) -> Self {
            Self {
                bits: self.bits ^ other.bits,
            }
        }
        /// Returns the complement of this set of flags.
        ///
        /// Specifically, the returned set contains all the flags which are
        /// not set in `self`, but which are allowed for this type.
        ///
        /// Alternatively, it can be thought of as the set difference
        /// between [`Self::all()`] and `self` (e.g. `Self::all() - self`)
        ///
        /// This is equivalent to using the `!` operator (e.g.
        /// [`ops::Not`]), as in `!flags`.
        ///
        /// [`Self::all()`]: Self::all
        /// [`ops::Not`]: https://doc.rust-lang.org/std/ops/trait.Not.html
        #[inline]
        #[must_use]
        pub const fn complement(self) -> Self {
            Self::from_bits_truncate(!self.bits)
        }
    }
    impl ::bitflags::_core::ops::BitOr for RequestFlags {
        type Output = Self;
        /// Returns the union of the two sets of flags.
        #[inline]
        fn bitor(self, other: RequestFlags) -> Self {
            Self {
                bits: self.bits | other.bits,
            }
        }
    }
    impl ::bitflags::_core::ops::BitOrAssign for RequestFlags {
        /// Adds the set of flags.
        #[inline]
        fn bitor_assign(&mut self, other: Self) {
            self.bits |= other.bits;
        }
    }
    impl ::bitflags::_core::ops::BitXor for RequestFlags {
        type Output = Self;
        /// Returns the left flags, but with all the right flags toggled.
        #[inline]
        fn bitxor(self, other: Self) -> Self {
            Self {
                bits: self.bits ^ other.bits,
            }
        }
    }
    impl ::bitflags::_core::ops::BitXorAssign for RequestFlags {
        /// Toggles the set of flags.
        #[inline]
        fn bitxor_assign(&mut self, other: Self) {
            self.bits ^= other.bits;
        }
    }
    impl ::bitflags::_core::ops::BitAnd for RequestFlags {
        type Output = Self;
        /// Returns the intersection between the two sets of flags.
        #[inline]
        fn bitand(self, other: Self) -> Self {
            Self {
                bits: self.bits & other.bits,
            }
        }
    }
    impl ::bitflags::_core::ops::BitAndAssign for RequestFlags {
        /// Disables all flags disabled in the set.
        #[inline]
        fn bitand_assign(&mut self, other: Self) {
            self.bits &= other.bits;
        }
    }
    impl ::bitflags::_core::ops::Sub for RequestFlags {
        type Output = Self;
        /// Returns the set difference of the two sets of flags.
        #[inline]
        fn sub(self, other: Self) -> Self {
            Self {
                bits: self.bits & !other.bits,
            }
        }
    }
    impl ::bitflags::_core::ops::SubAssign for RequestFlags {
        /// Disables all flags enabled in the set.
        #[inline]
        fn sub_assign(&mut self, other: Self) {
            self.bits &= !other.bits;
        }
    }
    impl ::bitflags::_core::ops::Not for RequestFlags {
        type Output = Self;
        /// Returns the complement of this set of flags.
        #[inline]
        fn not(self) -> Self {
            Self { bits: !self.bits } & Self::all()
        }
    }
    impl ::bitflags::_core::iter::Extend<RequestFlags> for RequestFlags {
        fn extend<T: ::bitflags::_core::iter::IntoIterator<Item = Self>>(
            &mut self,
            iterator: T,
        ) {
            for item in iterator {
                self.insert(item)
            }
        }
    }
    impl ::bitflags::_core::iter::FromIterator<RequestFlags> for RequestFlags {
        fn from_iter<T: ::bitflags::_core::iter::IntoIterator<Item = Self>>(
            iterator: T,
        ) -> Self {
            let mut result = Self::empty();
            result.extend(iterator);
            result
        }
    }
    /// Compute the necessary padding after `base` to have `align` alignment
    pub(crate) fn align_pad(base: usize, align: usize) -> usize {
        if true {
            if !align.is_power_of_two() {
                { ::std::rt::begin_panic("`align` must be a power of two") }
            }
        }
        let base = base as isize;
        let align = align as isize;
        (-base & (align - 1)) as usize
    }
}
