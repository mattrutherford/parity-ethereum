// Copyright 2015-2018 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Note that all the structs and functions here are documented in `parity.h`, to avoid
//! duplicating documentation.

extern crate futures;
extern crate panic_hook;
extern crate parity_ethereum;
extern crate tokio;
extern crate tokio_current_thread;

#[cfg(feature = "jni")]
extern crate jni;

use std::ffi::CString;
use std::os::raw::{c_char, c_void, c_int};
use std::{panic, ptr, slice, str, thread};
use std::sync::Arc;
use std::time::Duration;

use futures::Future;
use tokio_current_thread::CurrentThread;

#[cfg(feature = "jni")]
use std::mem;
#[cfg(feature = "jni")]
use jni::{JNIEnv, objects::JClass, objects::JString, sys::jlong, sys::jobjectArray};

type Callback = Option<extern "C" fn(*mut c_void, *const c_char, usize)>;

const QUERY_TIMEOUT: Duration = Duration::from_secs(5*60);

#[repr(C)]
pub struct ParityParams {
	pub configuration: *mut c_void,
	pub on_client_restart_cb: Callback,
	pub on_client_restart_cb_custom: *mut c_void,
}

#[no_mangle]
pub unsafe extern fn parity_config_from_cli(
	args: *const *const c_char,
	args_lens: *const usize,
	len: usize,
	output: *mut *mut c_void
) -> c_int {
	panic::catch_unwind(|| {
		*output = ptr::null_mut();

		let args = {
			let arg_ptrs = slice::from_raw_parts(args, len);
			let arg_lens = slice::from_raw_parts(args_lens, len);

			let mut args = Vec::with_capacity(len + 1);
			args.push("parity".to_owned());

			for (&arg, &len) in arg_ptrs.iter().zip(arg_lens.iter()) {
				let string = slice::from_raw_parts(arg as *const u8, len);
				match String::from_utf8(string.to_owned()) {
					Ok(a) => args.push(a),
					Err(_) => return 1,
				};
			}

			args
		};

		match parity_ethereum::Configuration::parse_cli(&args) {
			Ok(mut cfg) => {
				// Always disable the auto-updater when used as a library.
				cfg.args.arg_auto_update = "none".to_owned();

				let cfg = Box::into_raw(Box::new(cfg));
				*output = cfg as *mut _;
				0
			},
			Err(_) => {
				1
			},
		}
	}).unwrap_or(1)
}

#[no_mangle]
pub unsafe extern fn parity_config_destroy(cfg: *mut c_void) {
	let _ = panic::catch_unwind(|| {
		let _cfg = Box::from_raw(cfg as *mut parity_ethereum::Configuration);
	});
}

#[no_mangle]
pub unsafe extern fn parity_start(cfg: *const ParityParams, output: *mut *mut c_void) -> c_int {
	panic::catch_unwind(|| {
		*output = ptr::null_mut();
		let cfg: &ParityParams = &*cfg;

		let config = Box::from_raw(cfg.configuration as *mut parity_ethereum::Configuration);

		let on_client_restart_cb = {
			let cb = CallbackStr(cfg.on_client_restart_cb, cfg.on_client_restart_cb_custom);
			move |new_chain: String| { cb.call(&new_chain); }
		};

		let action = match parity_ethereum::start(*config, on_client_restart_cb, || {}) {
			Ok(action) => action,
			Err(_) => return 1,
		};

		match action {
			parity_ethereum::ExecutionAction::Instant(Some(s)) => { println!("{}", s); 0 },
			parity_ethereum::ExecutionAction::Instant(None) => 0,
			parity_ethereum::ExecutionAction::Running(client) => {
				*output = Box::into_raw(Box::<parity_ethereum::RunningClient>::new(client)) as *mut c_void;
				0
			}
		}
	}).unwrap_or(1)
}

#[no_mangle]
pub unsafe extern fn parity_destroy(client: *mut c_void) {
	let _ = panic::catch_unwind(|| {
		let client = Box::from_raw(client as *mut parity_ethereum::RunningClient);
		client.shutdown();
	});
}

fn to_cstring(response: Vec<u8>) -> (*mut c_char, usize) {
	let len = response.len();
	let cstr = CString::new(response).expect("valid string with no null bytes in the middle; qed").into_raw();
	(cstr, len)
}


#[no_mangle]
pub unsafe extern fn parity_rpc(
	client: *mut c_void,
	query: *const c_char,
	len: usize,
	callback: Callback,
) -> c_int {

	panic::catch_unwind(|| {

		let client: &mut parity_ethereum::RunningClient = &mut *(client as *mut parity_ethereum::RunningClient);

		let query_str = {
			let string = slice::from_raw_parts(query as *const u8, len);
			match str::from_utf8(string) {
				Ok(a) => a,
				Err(_) => return 1,
			}
		};

		let callback = match callback {
			Some(callback) => Arc::new(callback),
			None => return 1,
		};

		let cb = callback.clone();

		// FIXME: provide session object here, if we want to support the PubSub
		// [niklasad1]: I don't see the benefit with pubsub when we still have to wait for the future!
		let future = client.rpc_query(query_str, None).map(move |response| {
			let (cstring, len) = match response {
				Some(response) => to_cstring(response.into()),
				_ => to_cstring("empty response".into()),
			};
			cb(ptr::null_mut(), cstring, len);
			()
		});

		let _handle = thread::Builder::new()
			.name("rpc-subscriber".into())
			.spawn(move || {
				let mut current_thread = CurrentThread::new();
				current_thread.spawn(future);
				let _ = current_thread.run_timeout(QUERY_TIMEOUT).map_err(|_e| {
					let (cstring, len) = to_cstring("timeout".into());
					callback(ptr::null_mut(), cstring, len);
				});
			})
			.expect("rpc-subscriber thread shouldn't fail; qed");
		0
	}).unwrap_or(1)
}

#[no_mangle]
pub unsafe extern fn parity_set_panic_hook(callback: Callback, param: *mut c_void) {
	let cb = CallbackStr(callback, param);
	panic_hook::set_with(move |panic_msg| {
		cb.call(panic_msg);
	});
}

// Internal structure for handling callbacks that get passed a string.
struct CallbackStr(Callback, *mut c_void);
unsafe impl Send for CallbackStr {}
unsafe impl Sync for CallbackStr {}
impl CallbackStr {
	fn call(&self, new_chain: &str) {
		if let Some(ref cb) = self.0 {
			cb(self.1, new_chain.as_bytes().as_ptr() as *const _, new_chain.len())
		}
	}
}

#[cfg(feature = "jni")]
#[no_mangle]
pub unsafe extern "system" fn Java_io_parity_ethereum_Parity_configFromCli(env: JNIEnv, _: JClass, cli: jobjectArray) -> jlong {
	let cli_len = env.get_array_length(cli).expect("invalid Java bindings");

	let mut jni_strings = Vec::with_capacity(cli_len as usize);
	let mut opts = Vec::with_capacity(cli_len as usize);
	let mut opts_lens = Vec::with_capacity(cli_len as usize);

	for n in 0 .. cli_len {
		let elem = env.get_object_array_element(cli, n).expect("invalid Java bindings");
		let elem_str: JString = elem.into();
		match env.get_string(elem_str) {
			Ok(s) => {
				opts.push(s.as_ptr());
				opts_lens.push(s.to_bytes().len());
				jni_strings.push(s);
			},
			Err(err) => {
				let _ = env.throw_new("java/lang/Exception", err.to_string());
				return 0
			}
		};
	}

	let mut out = ptr::null_mut();
	match parity_config_from_cli(opts.as_ptr(), opts_lens.as_ptr(), cli_len as usize, &mut out) {
		0 => out as usize as jlong,
		_ => {
			let _ = env.throw_new("java/lang/Exception", "failed to create config object");
			0
		},
	}
}

#[cfg(feature = "jni")]
#[no_mangle]
pub unsafe extern "system" fn Java_io_parity_ethereum_Parity_build(env: JNIEnv, _: JClass, config: jlong) -> jlong {
	let params = ParityParams {
		configuration: config as usize as *mut c_void,
		.. mem::zeroed()
	};

	let mut out = ptr::null_mut();
	match parity_start(&params, &mut out) {
		0 => out as usize as jlong,
		_ => {
			let _ = env.throw_new("java/lang/Exception", "failed to start Parity");
			0
		},
	}
}

#[cfg(feature = "jni")]
#[no_mangle]
pub unsafe extern "system" fn Java_io_parity_ethereum_Parity_destroy(_env: JNIEnv, _: JClass, parity: jlong) {
	let parity = parity as usize as *mut c_void;
	parity_destroy(parity);
}

#[cfg(feature = "jni")]
#[no_mangle]
pub unsafe extern "system" fn Java_io_parity_ethereum_Parity_rpcQueryNative<'a>(env: JNIEnv<'a>, _: JClass, parity: jlong, rpc: JString) -> JString<'a> {
	let parity = parity as usize as *mut c_void;

	let rpc = match env.get_string(rpc) {
		Ok(s) => s,
		Err(err) => {
			let _ = env.throw_new("java/lang/Exception", err.to_string());
			return env.new_string("").expect("Creating an empty string never fails");
		},
	};

	let mut out_len = 255;
	let mut out = [0u8; 256];

	match parity_rpc(parity, rpc.as_ptr(), rpc.to_bytes().len(), out.as_mut_ptr() as *mut c_char, &mut out_len) {
		0 => (),
		_ => {
			let _ = env.throw_new("java/lang/Exception", "failed to perform RPC query");
			return env.new_string("").expect("Creating an empty string never fails");
		},
	}

	let out = str::from_utf8(&out[..out_len])
		.expect("parity always generates an UTF-8 RPC response");
	match env.new_string(out) {
		Ok(s) => s,
		Err(err) => {
			let _ = env.throw_new("java/lang/Exception", err.to_string());
			return env.new_string("").expect("Creating an empty string never fails");
		}
	}
}
