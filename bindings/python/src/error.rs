//! How cortex's failures arrive in Python.
//!
//! A console call fails one of two ways, and they are two exceptions because a caller acts
//! on them differently: [`Failure::Refused`] is the server answering with an error — a
//! timeout, a missing file — and carries the code it answered with, where
//! [`Failure::Broken`] is the channel itself gone, after which nothing more will be heard.
//! Both derive from `CortexError`, which is also what a failure with no finer class —
//! building a console, say — is raised as.
//!
//! The filesystem half answers in [`std::io::Error`], and pyo3 already raises that as the
//! matching `OSError` subclass, so it is left to do so.

use cortex::console::{Error, Failure};
use pyo3::{create_exception, exceptions::PyException, prelude::*, types::PyDict};

create_exception!(cortex, CortexError, PyException);
create_exception!(cortex, ConsoleRefused, CortexError);
create_exception!(cortex, ConsoleBroken, CortexError);

pub fn failure(failure: Failure) -> PyErr {
    match failure {
        Failure::Refused(error) => Python::attach(|py| {
            let err = ConsoleRefused::new_err(error.message);
            // `code` rather than an argument, so `str(err)` stays the server's message and
            // the number is read by name — against `ErrorCode`, which `register` exports.
            let _ = err.value(py).setattr("code", error.code);
            err
        }),
        Failure::Broken(error) => ConsoleBroken::new_err(format!("{error:#}")),
    }
}

/// What building a console answers in, which may be a [`Failure`] underneath: the server
/// refusing `init` is as much a refusal as it refusing an `exec`, and is raised as one.
pub fn anyhow(error: anyhow::Error) -> PyErr {
    match error.downcast::<Failure>() {
        Ok(f) => failure(f),
        Err(error) => CortexError::new_err(format!("{error:#}")),
    }
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add("CortexError", py.get_type::<CortexError>())?;
    m.add("ConsoleRefused", py.get_type::<ConsoleRefused>())?;
    m.add("ConsoleBroken", py.get_type::<ConsoleBroken>())?;

    // The numbers a `ConsoleRefused` carries, by name. Exported from here rather than
    // written again in Python so that there is one list of them, and it is cortex's.
    let codes = PyDict::new(py);
    for (name, code) in [
        ("TIMED_OUT", Error::TIMED_OUT),
        ("NOT_EXECUTABLE", Error::NOT_EXECUTABLE),
        ("BOOT_FAILED", Error::BOOT_FAILED),
        ("NOT_FOUND", Error::NOT_FOUND),
        ("IS_A_DIRECTORY", Error::IS_A_DIRECTORY),
        ("IO_FAILED", Error::IO_FAILED),
        ("UNSUPPORTED_MOUNT", Error::UNSUPPORTED_MOUNT),
        ("MOUNT_FAILED", Error::MOUNT_FAILED),
        ("UNSUPPORTED_NETWORK", Error::UNSUPPORTED_NETWORK),
        ("UNSUPPORTED_IMAGE", Error::UNSUPPORTED_IMAGE),
        ("UNKNOWN_IMAGE", Error::UNKNOWN_IMAGE),
        ("UNSUPPORTED_MACHINE", Error::UNSUPPORTED_MACHINE),
        ("INVALID_REQUEST", Error::INVALID_REQUEST),
        ("METHOD_NOT_FOUND", Error::METHOD_NOT_FOUND),
        ("INVALID_PARAMS", Error::INVALID_PARAMS),
        ("INTERNAL_ERROR", Error::INTERNAL_ERROR),
    ] {
        codes.set_item(name, code)?;
    }
    m.add("ERROR_CODES", codes)?;
    Ok(())
}
