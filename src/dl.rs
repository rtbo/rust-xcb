use libc;

use crate::dl;
use std::ffi::{CStr, CString};

#[derive(Clone, Debug)]
pub struct Error {
    kind: ErrorKind,
    detail: String,
}

impl Error {
    fn detail(&self) -> &str {
        self.detail.as_ref()
    }

    fn kind(&self) -> ErrorKind {
        self.kind
    }

    fn new(kind: ErrorKind, detail: String) -> Error {
        Error { kind, detail }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        f.write_str(self.kind.as_str())?;
        if !self.detail.is_empty() {
            f.write_str(" (")?;
            f.write_str(self.detail.as_ref())?;
            f.write_str(")")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {
    fn description(&self) -> &str {
        self.kind.as_str()
    }
}

//
// DlErrorKind
//

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ErrorKind {
    Library,
    Symbol,
}

impl ErrorKind {
    fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Library => "opening library failed",
            ErrorKind::Symbol => "loading symbol failed",
        }
    }
}

pub(crate) struct Lib {
    handle: *mut libc::c_void,
}

impl Lib {
    pub(crate) fn open(name: &str) -> Result<Lib, Error> {
        unsafe {
            let cname = match CString::new(name) {
                Ok(cname) => cname,
                Err(_) => {
                    return Err(Error::new(
                        ErrorKind::Library,
                        String::from("library name contains NUL byte(s)"),
                    ));
                }
            };

            let handle = libc::dlopen(cname.as_ptr(), libc::RTLD_LAZY);

            if handle.is_null() {
                let msg = libc::dlerror();

                if msg.is_null() {
                    return Err(Error::new(ErrorKind::Library, String::new()));
                }

                let cmsg = CStr::from_ptr(msg as *const libc::c_char);
                let detail = cmsg.to_string_lossy().into_owned();
                return Err(Error::new(ErrorKind::Library, detail));
            }

            Ok(Lib {
                handle: handle as *mut libc::c_void,
            })
        }
    }

    pub(crate) fn open_multi(names: &[&str]) -> Result<Lib, Error> {
        assert!(!names.is_empty());

        let mut msgs = Vec::new();

        for name in names.iter() {
            match Lib::open(name) {
                Ok(lib) => {
                    return Ok(lib);
                }
                Err(err) => {
                    msgs.push(format!("{}", err));
                }
            }
        }

        let mut detail = String::new();

        for (i, msg) in msgs.iter().enumerate() {
            if i != 0 {
                detail.push_str("; ");
            }
            detail.push_str(msg.as_ref());
        }

        Err(Error::new(ErrorKind::Library, detail))
    }

    pub(crate) fn symbol(&self, name: &str) -> Result<*mut libc::c_void, Error> {
        unsafe {
            let cname = match CString::new(name) {
                Ok(cname) => cname,
                Err(_) => {
                    return Err(Error::new(
                        ErrorKind::Symbol,
                        String::from("symbol name contains NUL byte(s)"),
                    ));
                }
            };

            let sym = libc::dlsym(self.handle as *mut _, cname.as_ptr());

            if sym.is_null() {
                let msg = libc::dlerror();

                if msg.is_null() {
                    return Err(Error::new(ErrorKind::Symbol, String::from(name)));
                }

                let cmsg = CStr::from_ptr(msg as *const libc::c_char);
                let detail = format!("{} - {}", name, cmsg.to_string_lossy().into_owned());
                return Err(Error::new(ErrorKind::Symbol, detail));
            }

            Ok(sym as *mut libc::c_void)
        }
    }
}

impl Drop for Lib {
    fn drop(&mut self) {
        unsafe {
            libc::dlclose(self.handle as *mut _);
        }
    }
}
