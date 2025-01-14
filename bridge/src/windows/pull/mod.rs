use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
use std::ptr;
use std::thread;

use bridge_derive::{secret_string, secret_string_from_file};

use super::api;
use super::result::*;

use crate::deserialize::{deserialize_data, DeserializeError};
use crate::PullResult;

macro_rules! pipe_name {
  () => {
    secret_string!(r#"\\.\pipe\b62340b3-9f87-4f38-b844-7b8d1598b64b"#)
  };
}

pub fn get_error_json(msg: String) -> String {
  use serde_json::{self, json};
  serde_json::to_string(&json!({
    "error": msg,
  }))
  .unwrap()
}

fn run_client_script() -> BridgeResult<i32> {
  #[cfg(feature = "guild")]
  {
    api::run(&secret_string_from_file!("bridge/assets/client_guild.py"))
  }

  #[cfg(not(feature = "guild"))]
  {
    api::run(&secret_string_from_file!("bridge/assets/client_windows.py"))
  }
}

pub fn run_client() {
  let result = api::init().and_then(|_| run_client_script());

  if let Err(err) = result {
    debug!("client error: {}", err);
    unsafe {
      use std::ffi::CString;
      use winapi::shared::sddl::*;
      use winapi::shared::winerror::*;
      use winapi::um::errhandlingapi::*;
      use winapi::um::fileapi::*;
      use winapi::um::handleapi::*;
      use winapi::um::minwinbase::SECURITY_ATTRIBUTES;
      use winapi::um::winbase::*;
      use winapi::um::winnt::*;

      let pipe_name = pipe_name!();
      let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
      let sdstr = CString::new("(A;OICI;GRGWGX;;;AU)").unwrap();

      ConvertStringSecurityDescriptorToSecurityDescriptorA(
        sdstr.as_ptr(),
        SDDL_REVISION_1.into(),
        &mut sd as *mut PSECURITY_DESCRIPTOR,
        ptr::null_mut(),
      );

      let mut sattrs = SECURITY_ATTRIBUTES {
        nLength: ::std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
      };

      loop {
        let pipe = CreateFileA(
          CString::new(&pipe_name as &str).unwrap().as_ptr(),
          GENERIC_WRITE | GENERIC_READ,
          0,
          if sd != ptr::null_mut() {
            &mut sattrs as *mut SECURITY_ATTRIBUTES
          } else {
            ptr::null_mut()
          },
          OPEN_EXISTING,
          0,
          ptr::null_mut(),
        );

        if pipe == INVALID_HANDLE_VALUE {
          debug!("open pipe error.");
          break;
        }

        if GetLastError() == ERROR_PIPE_BUSY {
          if WaitNamedPipeA(CString::new(&pipe_name as &str).unwrap().as_ptr(), 2000) == 0 {
            // wait timeout
            debug!("wait pipe timeout.");
            break;
          } else {
            continue;
          }
        }

        let err_msg = get_error_json(err.to_string());
        let bytes = err_msg.as_bytes();

        let mut pos = 0;

        loop {
          let mut written: u32 = 0;
          let remain_size = bytes.len() - pos;

          let ok = WriteFile(
            pipe,
            ::std::mem::transmute((&bytes[pos..]).as_ptr()),
            remain_size as u32,
            &mut written as *mut u32,
            ptr::null_mut(),
          ) == 1;

          if !ok {
            debug!("write pipe error: {}", GetLastError());
            break;
          }

          pos = pos + written as usize;
          if pos == bytes.len() {
            break;
          }
        }

        CloseHandle(pipe);
      }
    }
  }

  debug!("terminating...");
  unsafe {
    ::winapi::um::processthreadsapi::TerminateProcess(
      ::winapi::um::processthreadsapi::GetCurrentProcess(),
      0,
    );
  }
}

pub fn run_server() -> PullResult {
  use super::{get_env, inject};

  let env = get_env().unwrap();
  let dll_path = env.self_path;

  let (cmd_s, cmd_r) = bounded(1);
  let (rep_s, rep_r) = bounded(1);

  let worker = thread::spawn(move || pipe_server_worker(rep_s, cmd_r));

  match rep_r.recv().unwrap() {
    PipeMsg::ServerStarted => {}
    PipeMsg::ServerError(err) => return PullResult::err(&format!("Worker error: {}", err)),
    _ => return PullResult::err(&format!("Unexpected message type.")),
  };

  println!("连接中...");

  debug!("pipe server started, injecting {:?}...", dll_path);
  let remote_err = if let Err(err) = inject::inject_dll_to_yys(dll_path) {
    Some(err.to_string())
  } else {
    None
  };

  if remote_err.is_some() {
    debug!("client error: {}", remote_err.clone().unwrap());
    cmd_s.send(PipeMsg::CmdTerm).unwrap();
  }

  unsafe {
    use std::os::windows::io::AsRawHandle;
    use winapi::shared::winerror::*;
    use winapi::um::errhandlingapi::*;
    let mut retry = 0;
    loop {
      let r = ::winapi::um::ioapiset::CancelSynchronousIo(worker.as_raw_handle());
      if r == 1 {
        break;
      }
      let last_err = GetLastError();
      if r != 1 && last_err == ERROR_NOT_FOUND {
        if retry == 3 {
          break;
        }
        thread::sleep(::std::time::Duration::from_millis(200));
        debug!("waiting for cancel...");
        retry = retry + 1;
      } else {
        panic!("Unknown worker error: {}", last_err);
      }
    }
  }

  match rep_r.recv().unwrap() {
    PipeMsg::ServerStopped { err, data } => {
      debug!("pipe server stopped.");
      if let Some(err) = err {
        if let Some(remote_err) = remote_err {
          PullResult::err(&remote_err.to_string())
        } else {
          PullResult::err(&err)
        }
      } else {
        match deserialize_data(&data) {
          Ok(data) => PullResult::ok(data),
          Err(DeserializeError::ParseSnapshotData(msg)) => PullResult::err_with_data(&msg, data),
        }
      }
    }
    _ => PullResult::err(&format!("Unexpected message type.")),
  }
}

enum PipeMsg {
  ServerStarted,
  ServerError(String),
  ServerStopped { err: Option<String>, data: Vec<u8> },
  CmdTerm,
}

fn pipe_server_worker(s: Sender<PipeMsg>, r: Receiver<PipeMsg>) {
  use bridge_derive::secret_string;
  let pipe_path = pipe_name!();

  enum ErrorCode {
    CreatePipe = 1,
  }

  unsafe {
    use std::ffi::CString;
    use std::ptr;
    use winapi::shared::winerror::*;
    use winapi::um::errhandlingapi::*;
    use winapi::um::fileapi::*;
    use winapi::um::handleapi::*;
    use winapi::um::namedpipeapi::*;
    use winapi::um::winbase::*;

    const BUFFER_SIZE: u32 = 1024 * 1024;

    let pipe = CreateNamedPipeA(
      CString::new(pipe_path).unwrap().as_ptr(),
      PIPE_ACCESS_DUPLEX,
      PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
      1,
      BUFFER_SIZE,
      BUFFER_SIZE,
      0,
      ptr::null_mut(),
    );

    if pipe == INVALID_HANDLE_VALUE {
      s.send(PipeMsg::ServerError(format!(
        "{},{}",
        ErrorCode::CreatePipe as i32,
        GetLastError()
      )))
      .unwrap();
      return;
    }

    s.send(PipeMsg::ServerStarted).unwrap();

    let connected =
      ConnectNamedPipe(pipe, ptr::null_mut()) == 1 || GetLastError() == ERROR_PIPE_CONNECTED;

    debug!("connected = {}", connected);

    println!("读取中...");

    let mut terminated = !connected
      || match r.try_recv() {
        Ok(PipeMsg::CmdTerm) => {
          debug!("termination request received.");
          true
        }
        Err(TryRecvError::Empty) => false,
        Err(TryRecvError::Disconnected) => {
          debug!("channel disconnected.");
          true
        }
        _ => false,
      };

    let mut data = vec![];
    let mut last_err = 0;
    if !terminated {
      loop {
        let mut buf: Vec<u8> = Vec::with_capacity(BUFFER_SIZE as usize);
        buf.resize(BUFFER_SIZE as usize, 0);
        let mut bytes_read: u32 = 0;
        let ok = ReadFile(
          pipe,
          buf.as_mut_ptr() as *mut winapi::ctypes::c_void,
          BUFFER_SIZE,
          &mut bytes_read as *mut u32,
          ptr::null_mut(),
        );
        if ok != 1 || bytes_read == 0 {
          match GetLastError() {
            ERROR_BROKEN_PIPE => {
              debug!("client disconnected.");
              DisconnectNamedPipe(pipe);
              break;
            }
            ERROR_MORE_DATA => {}
            code => {
              last_err = code;
              terminated = true;
              debug!("ReadFile error: {}", last_err);
              break;
            }
          }
        }

        debug!("bytes read = {}", bytes_read);
        data.extend(&buf[..(bytes_read as usize)]);
      }
    }

    CloseHandle(pipe);
    debug!("pipe closed.");
    s.send(PipeMsg::ServerStopped {
      err: if terminated {
        Some(format!(
          "进程异常终止，请确保你使用的是官网最新版的阴阳师: {}",
          last_err
        ))
      } else {
        None
      },
      data,
    })
    .unwrap();
  }
}
