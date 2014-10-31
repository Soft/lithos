use std::str::from_utf8;
use std::io::{IoError, IoResult, InvalidInput};
use std::io::net::ip::IpAddr;
use std::io::net::get_host_addresses;
use libc::{c_int, size_t, c_char};


extern {
    pub fn gethostname(name: *mut c_char, size: size_t) -> c_int;
}

pub fn get_host_ip() -> IoResult<IpAddr> {
    let host = try!(get_host_name());
    println!("NAME {}", host);
    let addr = try!(get_host_addresses(host.as_slice()));
    println!("ADDR {}", addr);
    return Ok(addr[0]);
}

pub fn get_host_name() -> IoResult<String> {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let nbytes = unsafe {
        buf.set_len(256);
        gethostname(
            buf.as_mut_slice().as_mut_ptr() as *mut i8,
            256)
    };
    if nbytes != 0 {
        return Err(IoError::last_error());
    }
    return buf.as_slice().splitn(1, |x| *x == 0u8)
           .next().and_then(from_utf8).map(|x| x.to_string())
           .ok_or(IoError {
                kind: InvalidInput,
                desc: "Got invalid hostname from OS",
                detail: None,
            });
}