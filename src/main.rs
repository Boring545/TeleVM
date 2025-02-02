// Copyright (c) 2020 Huawei Technologies Co.,Ltd. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
//
// Copyright (c) 2023 China Telecom Co.,Ltd. All rights reserved.
// 
// Modifications made by China Telecom Co.,Ltd:
// - Modify main for risc-v architecture
//
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::{Arc, Mutex};
use std::path::Path;
use std::fs::File;

use anyhow::{bail, Context, Result};
use log::{error, info};
use machine::{LightMachine, MachineOps};
use machine_manager::{
    cmdline::{check_api_channel, create_args_parser, create_vmconfig},
    config::MachineType,
    config::VmConfig,
    event_loop::EventLoop,
    qmp::QmpChannel,
    signal_handler::{exit_with_code, register_kill_signal, VM_EXIT_GENE_ERR},
    socket::Socket,
    temp_cleaner::TempCleaner,
    test_server::TestSock,
};
use util::loop_context::EventNotifierHelper;
use util::test_helper::{is_test_enabled, set_test_enabled};
use util::{arg_parser, daemonize::daemonize, logger, set_termi_canon_mode};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum MainError {
    #[error("Manager")]
    Manager {
        #[from]
        source: machine_manager::error::MachineManagerError,
    },
    #[error("Util")]
    Util {
        #[from]
        source: util::error::UtilError,
    },
    #[error("Machine")]
    Machine {
        #[from]
        source: machine::error::MachineError,
    },
    #[error("Io")]
    Io {
        #[from]
        source: std::io::Error,
    },
}

pub trait ExitCode {
    /// Returns the value to use as the exit status.
    fn code(self) -> i32;
}

impl ExitCode for i32 {
    fn code(self) -> i32 {
        self
    }
}

impl ExitCode for () {
    fn code(self) -> i32 {
        0
    }
}

fn main() {
    ::std::process::exit(match run() {
        Ok(ret) => ExitCode::code(ret),
        Err(ref e) => {
            write!(&mut ::std::io::stderr(), "{}", format!("{:?}\r\n", e))
                .expect("Error writing to stderr");

            1
        }
    });
}

fn run() -> Result<()> {
    let cmd_args = create_args_parser().get_matches()?;

    if cmd_args.is_present("mod-test") {
        set_test_enabled();
    }

    if let Some(logfile_path) = cmd_args.value_of("display log") {
        if logfile_path.is_empty() {
            logger::init_logger_with_env(Some(Box::new(std::io::stdout())))
                .with_context(|| "Failed to init logger.")?;
        } else {
            let logfile = std::fs::OpenOptions::new()
                .read(false)
                .write(true)
                .append(true)
                .create(true)
                .mode(0o640)
                .open(logfile_path)
                .with_context(|| "Failed to open log file")?;
            logger::init_logger_with_env(Some(Box::new(logfile)))
                .with_context(|| "Failed to init logger.")?;
        }
    }

    std::panic::set_hook(Box::new(|panic_msg| {
        set_termi_canon_mode().expect("Failed to set terminal to canonical mode.");

        let panic_file = panic_msg.location().map_or("", |loc| loc.file());
        let panic_line = panic_msg.location().map_or(0, |loc| loc.line());
        if let Some(msg) = panic_msg.payload().downcast_ref::<&str>() {
            error!("Panic at [{}: {}]: {}.", panic_file, panic_line, msg);
        } else {
            error!("Panic at [{}: {}].", panic_file, panic_line);
        }

        // clean temporary file
        TempCleaner::clean();
        exit_with_code(VM_EXIT_GENE_ERR);
    }));

    let mut vm_config: VmConfig = create_vmconfig(&cmd_args)?;
    info!("VmConfig is {:?}", vm_config);

    match real_main(&cmd_args, &mut vm_config) {
        Ok(()) => {
            info!("MainLoop over, Vm exit");
            // clean temporary file
            TempCleaner::clean();
        }
        Err(ref e) => {
            println!("exit at real_main err");
            set_termi_canon_mode().expect("Failed to set terminal to canonical mode.");
            if cmd_args.is_present("display log") {
                error!("{}", format!("{:?}", e));
            } else {
                write!(&mut std::io::stderr(), "{}", format!("{:?}", e))
                    .expect("Failed to write to stderr");
            }
            // clean temporary file
            TempCleaner::clean();
            exit_with_code(VM_EXIT_GENE_ERR);
        }
    }

    Ok(())
}

fn real_main(cmd_args: &arg_parser::ArgMatches, vm_config: &mut VmConfig) -> Result<()> {
    TempCleaner::object_init();

    if cmd_args.is_present("daemonize") {
        match daemonize(cmd_args.value_of("pidfile")) {
            Ok(()) => {
                if let Some(pidfile) = cmd_args.value_of("pidfile") {
                    TempCleaner::add_path(pidfile);
                }
                info!("Daemonize mode start!");
            }
            Err(e) => bail!("Daemonize start failed: {}", e),
        }
    } else if cmd_args.value_of("pidfile").is_some() {
        bail!("-pidfile must be used with -daemonize together.");
    }

    QmpChannel::object_init();
    EventLoop::object_init(&vm_config.iothreads)?;
    register_kill_signal();

    let listeners = check_api_channel(cmd_args, vm_config)?;
    let mut sockets = Vec::new();
    let vm: Arc<Mutex<dyn MachineOps + Send + Sync>> = match vm_config.machine_config.mach_type {
        MachineType::MicroVm => {
            let vm = Arc::new(Mutex::new(
                LightMachine::new(vm_config).with_context(|| "Failed to init MicroVM")?,
            ));
            MachineOps::realize(&vm, vm_config).with_context(|| "Failed to realize micro VM.")?;
            EventLoop::set_manager(vm.clone(), None);

            // if cmd_args.is_present("mod-test") {
            if is_test_enabled() {
                let sock_path = cmd_args.value_of("mod-test").unwrap();
                println!("[[ successfully test_enabled ]], sock_path is {} ", &sock_path);

                // 检查socket文件是否存在，如果不存在则创建
                // if !Path::new(&sock_path).exists() {
                    // println!("sock_path does not exist");
                    // match File::create(&sock_path) {
                    //     Ok(_) => {
                    //         println!("Socket file created: {}", &sock_path)
                    //     },
                    //     Err(e) => {
                    //         eprintln!("Failed to create socket file: {}", e)
                    //     },
                    // }
                // }

                let test_sock = Some(TestSock::new(sock_path.as_str(), vm.clone()));
                EventLoop::update_event(
                    EventNotifierHelper::internal_notifiers(Arc::new(Mutex::new(
                        test_sock.unwrap(),
                    ))),
                    None,
                )
                .with_context(|| "Failed to add test socket to MainLoop")?;
            }

            for listener in listeners {
                sockets.push(Socket::from_unix_listener(listener, Some(vm.clone())));
            }
            vm
        }
        MachineType::None => {
            let vm = Arc::new(Mutex::new(
                LightMachine::new(vm_config).with_context(|| "Failed to init NoneVM")?,
            ));
            EventLoop::set_manager(vm.clone(), None);
            for listener in listeners {
                sockets.push(Socket::from_unix_listener(listener, Some(vm.clone())));
            }
            vm
        }
    };

    for socket in sockets {
        EventLoop::update_event(
            EventNotifierHelper::internal_notifiers(Arc::new(Mutex::new(socket))),
            None,
        )
        .with_context(|| "Failed to add api event to MainLoop")?;
    }

    machine::vm_run(&vm, cmd_args).with_context(|| "Failed to start VM.")?;

    EventLoop::loop_run().with_context(|| "MainLoop exits unexpectedly: error occurs")?;
    Ok(())
}
