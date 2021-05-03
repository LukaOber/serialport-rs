use std::convert::TryFrom;
use std::ffi::OsStr;
use std::mem::MaybeUninit;
use std::os::windows::prelude::*;
use std::path::Path;
use std::time::Duration;
use std::{io, mem, ptr};

use winapi::shared::minwindef::*;
use winapi::um::commapi::*;
use winapi::um::fileapi::*;
use winapi::um::handleapi::*;
use winapi::um::processthreadsapi::GetCurrentProcess;
use winapi::um::winbase::*;
use winapi::um::winnt::{
    DUPLICATE_SAME_ACCESS, FILE_ATTRIBUTE_NORMAL, GENERIC_READ, GENERIC_WRITE, HANDLE,
};

use crate::sys::windows::dcb;
use crate::{
    ClearBuffer, DataBits, Error, ErrorKind, FlowControl, Parity, Result, SerialPortBuilder,
    StopBits,
};

/// A serial port implementation for Windows COM ports
///
/// The port will be closed when the value is dropped. However, this struct
/// should not be instantiated directly by using `SerialPort::open()`, instead use
/// the cross-platform `serialport::open()` or
/// `serialport::open_with_settings()`.
#[derive(Debug)]
pub struct SerialPort {
    handle: HANDLE,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    port_name: Option<String>,
}

unsafe impl Send for SerialPort {}

impl SerialPort {
    /// Opens a COM port as a serial device.
    ///
    /// `port` should be the name of a COM port, e.g., `COM1`.
    ///
    /// If the COM port handle needs to be opened with special flags, use
    /// `from_raw_handle` method to create the `SerialPort`. Note that you should
    /// set the different settings before using the serial port using `set_all`.
    ///
    /// ## Errors
    ///
    /// * `NoDevice` if the device could not be opened. This could indicate that
    ///    the device is already in use.
    /// * `InvalidInput` if `port` is not a valid device name.
    /// * `Io` for any other I/O error while opening or initializing the device.
    pub fn open(builder: SerialPortBuilder, path: impl AsRef<Path>) -> Result<SerialPort> {
        let path = path.as_ref();

        let name: Vec<u16> = OsStr::new(r"\\.\")
            .encode_wide()
            .chain(path.as_os_str().encode_wide())
            .chain(std::iter::once(0))
            .collect();

        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                0 as HANDLE,
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            return Err(super::error::last_os_error());
        }

        let mut dcb = dcb::get_dcb(handle)?;
        dcb::init(&mut dcb);
        dcb::set_baud_rate(&mut dcb, builder.baud_rate);
        dcb::set_data_bits(&mut dcb, builder.data_bits);
        dcb::set_parity(&mut dcb, builder.parity);
        dcb::set_stop_bits(&mut dcb, builder.stop_bits);
        dcb::set_flow_control(&mut dcb, builder.flow_control);
        dcb::set_dcb(handle, dcb)?;

        let mut com = SerialPort::open_from_raw_handle(handle as RawHandle);
        com.set_timeouts(builder.read_timeout, builder.write_timeout)?;
        com.port_name = Some(path.to_string_lossy().into_owned());
        Ok(com)
    }

    /// Attempts to clone the `SerialPort`. This allow you to write and read simultaneously from the
    /// same serial connection. Please note that if you want a real asynchronous serial port you
    /// should look at [mio-serial](https://crates.io/crates/mio-serial) or
    /// [tokio-serial](https://crates.io/crates/tokio-serial).
    ///
    /// Also, you must be very careful when changing the settings of a cloned `SerialPort` : since
    /// the settings are cached on a per object basis, trying to modify them from two different
    /// objects can cause some nasty behavior.
    ///
    /// # Errors
    ///
    /// This function returns an error if the serial port couldn't be cloned.
    pub fn try_clone(&self) -> Result<Self> {
        let process_handle: HANDLE = unsafe { GetCurrentProcess() };
        let mut cloned_handle: HANDLE = INVALID_HANDLE_VALUE;
        unsafe {
            DuplicateHandle(
                process_handle,
                self.handle,
                process_handle,
                &mut cloned_handle,
                0,
                TRUE,
                DUPLICATE_SAME_ACCESS,
            );
            if cloned_handle != INVALID_HANDLE_VALUE {
                Ok(SerialPort {
                    handle: cloned_handle,
                    port_name: self.port_name.clone(),
                    read_timeout: self.read_timeout,
                    write_timeout: self.write_timeout,
                })
            } else {
                Err(super::error::last_os_error())
            }
        }
    }

    fn escape_comm_function(&mut self, function: DWORD) -> Result<()> {
        match unsafe { EscapeCommFunction(self.handle, function) } {
            0 => Err(super::error::last_os_error()),
            _ => Ok(()),
        }
    }

    fn read_pin(&mut self, pin: DWORD) -> Result<bool> {
        let mut status: DWORD = 0;

        match unsafe { GetCommModemStatus(self.handle, &mut status) } {
            0 => Err(super::error::last_os_error()),
            _ => Ok(status & pin != 0),
        }
    }

    fn open_from_raw_handle(handle: RawHandle) -> Self {
        SerialPort {
            handle: handle as HANDLE,
            // It's possible to retrieve the COMMTIMEOUTS struct from the handle,
            // but mapping that back to simple timeout durations would be difficult.
            // Instead we just set `None` and add a warning to `FromRawHandle`.
            read_timeout: None,
            write_timeout: None,
            // It is not trivial to get the file path corresponding to a handle.
            // We'll punt and set it `None` here.
            port_name: None,
        }
    }

    pub fn name(&self) -> Option<&str> {
        self.port_name.as_ref().map(|s| &**s)
    }

    pub fn read_timeout(&self) -> Option<Duration> {
        self.read_timeout
    }

    pub fn write_timeout(&self) -> Option<Duration> {
        self.write_timeout
    }

    pub fn set_read_timeout(&mut self, read_timeout: Option<Duration>) -> Result<()> {
        self.set_timeouts(read_timeout, self.write_timeout)
    }

    pub fn set_write_timeout(&mut self, write_timeout: Option<Duration>) -> Result<()> {
        self.set_timeouts(self.read_timeout, write_timeout)
    }

    fn set_timeouts(
        &mut self,
        read_timeout: Option<Duration>,
        write_timeout: Option<Duration>,
    ) -> Result<()> {
        let read_timeout_ms = match read_timeout {
            Some(duration) => {
                DWORD::try_from(duration.as_millis()).map_or(DWORD::MAX, |timeout| timeout.max(1))
            }
            None => 0,
        };

        let write_timeout_ms = match write_timeout {
            Some(duration) => {
                DWORD::try_from(duration.as_millis()).map_or(DWORD::MAX, |timeout| timeout.max(1))
            }
            None => 0,
        };

        let mut timeouts = COMMTIMEOUTS {
            ReadIntervalTimeout: 1,
            ReadTotalTimeoutMultiplier: 0,
            ReadTotalTimeoutConstant: read_timeout_ms,
            WriteTotalTimeoutMultiplier: 0,
            WriteTotalTimeoutConstant: write_timeout_ms,
        };

        if unsafe { SetCommTimeouts(self.handle, &mut timeouts) } == 0 {
            return Err(super::error::last_os_error());
        }

        self.read_timeout = read_timeout;
        self.write_timeout = write_timeout;
        Ok(())
    }

    pub fn write_request_to_send(&mut self, level: bool) -> Result<()> {
        if level {
            self.escape_comm_function(SETRTS)
        } else {
            self.escape_comm_function(CLRRTS)
        }
    }

    pub fn write_data_terminal_ready(&mut self, level: bool) -> Result<()> {
        if level {
            self.escape_comm_function(SETDTR)
        } else {
            self.escape_comm_function(CLRDTR)
        }
    }

    pub fn read_clear_to_send(&mut self) -> Result<bool> {
        self.read_pin(MS_CTS_ON)
    }

    pub fn read_data_set_ready(&mut self) -> Result<bool> {
        self.read_pin(MS_DSR_ON)
    }

    pub fn read_ring_indicator(&mut self) -> Result<bool> {
        self.read_pin(MS_RING_ON)
    }

    pub fn read_carrier_detect(&mut self) -> Result<bool> {
        self.read_pin(MS_RLSD_ON)
    }

    pub fn baud_rate(&self) -> Result<u32> {
        let dcb = dcb::get_dcb(self.handle)?;
        Ok(dcb.BaudRate as u32)
    }

    pub fn data_bits(&self) -> Result<DataBits> {
        let dcb = dcb::get_dcb(self.handle)?;
        match dcb.ByteSize {
            5 => Ok(DataBits::Five),
            6 => Ok(DataBits::Six),
            7 => Ok(DataBits::Seven),
            8 => Ok(DataBits::Eight),
            _ => Err(Error::new(
                ErrorKind::Unknown,
                "Invalid data bits setting encountered",
            )),
        }
    }

    pub fn parity(&self) -> Result<Parity> {
        let dcb = dcb::get_dcb(self.handle)?;
        match dcb.Parity {
            ODDPARITY => Ok(Parity::Odd),
            EVENPARITY => Ok(Parity::Even),
            NOPARITY => Ok(Parity::None),
            _ => Err(Error::new(
                ErrorKind::Unknown,
                "Invalid parity bits setting encountered",
            )),
        }
    }

    pub fn stop_bits(&self) -> Result<StopBits> {
        let dcb = dcb::get_dcb(self.handle)?;
        match dcb.StopBits {
            TWOSTOPBITS => Ok(StopBits::Two),
            ONESTOPBIT => Ok(StopBits::One),
            _ => Err(Error::new(
                ErrorKind::Unknown,
                "Invalid stop bits setting encountered",
            )),
        }
    }

    pub fn flow_control(&self) -> Result<FlowControl> {
        let dcb = dcb::get_dcb(self.handle)?;
        if dcb.fOutxCtsFlow() != 0 || dcb.fRtsControl() != 0 {
            Ok(FlowControl::Hardware)
        } else if dcb.fOutX() != 0 || dcb.fInX() != 0 {
            Ok(FlowControl::Software)
        } else {
            Ok(FlowControl::None)
        }
    }

    pub fn set_baud_rate(&mut self, baud_rate: u32) -> Result<()> {
        let mut dcb = dcb::get_dcb(self.handle)?;
        dcb::set_baud_rate(&mut dcb, baud_rate);
        dcb::set_dcb(self.handle, dcb)
    }

    pub fn set_data_bits(&mut self, data_bits: DataBits) -> Result<()> {
        let mut dcb = dcb::get_dcb(self.handle)?;
        dcb::set_data_bits(&mut dcb, data_bits);
        dcb::set_dcb(self.handle, dcb)
    }

    pub fn set_parity(&mut self, parity: Parity) -> Result<()> {
        let mut dcb = dcb::get_dcb(self.handle)?;
        dcb::set_parity(&mut dcb, parity);
        dcb::set_dcb(self.handle, dcb)
    }

    pub fn set_stop_bits(&mut self, stop_bits: StopBits) -> Result<()> {
        let mut dcb = dcb::get_dcb(self.handle)?;
        dcb::set_stop_bits(&mut dcb, stop_bits);
        dcb::set_dcb(self.handle, dcb)
    }

    pub fn set_flow_control(&mut self, flow_control: FlowControl) -> Result<()> {
        let mut dcb = dcb::get_dcb(self.handle)?;
        dcb::set_flow_control(&mut dcb, flow_control);
        dcb::set_dcb(self.handle, dcb)
    }

    pub fn bytes_to_read(&self) -> Result<u32> {
        let mut errors: DWORD = 0;
        let mut comstat = MaybeUninit::uninit();

        if unsafe { ClearCommError(self.handle, &mut errors, comstat.as_mut_ptr()) != 0 } {
            unsafe { Ok(comstat.assume_init().cbInQue) }
        } else {
            Err(super::error::last_os_error())
        }
    }

    pub fn bytes_to_write(&self) -> Result<u32> {
        let mut errors: DWORD = 0;
        let mut comstat = MaybeUninit::uninit();

        if unsafe { ClearCommError(self.handle, &mut errors, comstat.as_mut_ptr()) != 0 } {
            unsafe { Ok(comstat.assume_init().cbOutQue) }
        } else {
            Err(super::error::last_os_error())
        }
    }

    pub fn clear(&self, buffer_to_clear: ClearBuffer) -> Result<()> {
        let buffer_flags = match buffer_to_clear {
            ClearBuffer::Input => PURGE_RXABORT | PURGE_RXCLEAR,
            ClearBuffer::Output => PURGE_TXABORT | PURGE_TXCLEAR,
            ClearBuffer::All => PURGE_RXABORT | PURGE_RXCLEAR | PURGE_TXABORT | PURGE_TXCLEAR,
        };

        if unsafe { PurgeComm(self.handle, buffer_flags) != 0 } {
            Ok(())
        } else {
            Err(super::error::last_os_error())
        }
    }

    pub fn set_break(&self) -> Result<()> {
        if unsafe { SetCommBreak(self.handle) != 0 } {
            Ok(())
        } else {
            Err(super::error::last_os_error())
        }
    }

    pub fn clear_break(&self) -> Result<()> {
        if unsafe { ClearCommBreak(self.handle) != 0 } {
            Ok(())
        } else {
            Err(super::error::last_os_error())
        }
    }
}

impl Drop for SerialPort {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

impl AsRawHandle for SerialPort {
    fn as_raw_handle(&self) -> RawHandle {
        self.handle as RawHandle
    }
}

impl AsRawHandle for crate::SerialPort {
    fn as_raw_handle(&self) -> RawHandle {
        self.0.as_raw_handle()
    }
}

impl IntoRawHandle for SerialPort {
    fn into_raw_handle(self) -> RawHandle {
        let handle = self.handle as RawHandle;
        // Forget self to avoid running the destructor.
        mem::forget(self);
        handle
    }
}

impl IntoRawHandle for crate::SerialPort {
    fn into_raw_handle(self) -> RawHandle {
        // crate::SerialPort doesn't explicitly implement Drop, so we can just take
        // out the inner value.
        self.0.into_raw_handle()
    }
}

impl FromRawHandle for SerialPort {
    unsafe fn from_raw_handle(handle: RawHandle) -> Self {
        SerialPort::open_from_raw_handle(handle)
    }
}

impl FromRawHandle for crate::SerialPort {
    /// Create a SerialPort from a raw handle.
    ///
    /// Warning: the returned `SerialPort` will report timeouts of `None` for
    /// `read_timeout` and `write_timeout`, however the actual timeouts set on the
    /// underlying handle may be different. You can use `set_read_timeout` and
    /// `set_write_timeout` to reset the timeouts on the handle to make them match
    /// the values on the `SerialPort`.
    unsafe fn from_raw_handle(handle: RawHandle) -> Self {
        crate::SerialPort(SerialPort::from_raw_handle(handle))
    }
}

impl io::Read for &SerialPort {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut len: DWORD = 0;

        match unsafe {
            ReadFile(
                self.handle,
                buf.as_mut_ptr() as LPVOID,
                buf.len() as DWORD,
                &mut len,
                ptr::null_mut(),
            )
        } {
            0 => Err(io::Error::last_os_error()),
            _ => {
                if len != 0 {
                    Ok(len as usize)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Operation timed out",
                    ))
                }
            }
        }
    }
}

impl io::Write for &SerialPort {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut len: DWORD = 0;

        match unsafe {
            WriteFile(
                self.handle,
                buf.as_ptr() as LPVOID,
                buf.len() as DWORD,
                &mut len,
                ptr::null_mut(),
            )
        } {
            0 => Err(io::Error::last_os_error()),
            _ => Ok(len as usize),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match unsafe { FlushFileBuffers(self.handle) } {
            0 => Err(io::Error::last_os_error()),
            _ => Ok(()),
        }
    }
}
