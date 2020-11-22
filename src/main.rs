#![windows_subsystem = "windows"]

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::process;
use std::rc::Rc;
use std::thread;

use crc32c;
use crossbeam::channel::unbounded;
use directories_next::ProjectDirs;
use eyre::{eyre, Result};
use iui::controls::{Label, VerticalBox};
use iui::prelude::*;
use lazy_static::lazy_static;
use native_dialog::*;
use reqwest::StatusCode;
use scopeguard::{defer, defer_on_unwind};
use serde::{Deserialize, Serialize};

lazy_static! {
    static ref HTTP_CLIENT: reqwest::blocking::Client = reqwest::blocking::Client::new();
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
struct AppEntry {
    name: String,
    dir: std::path::PathBuf,
    patch: u16,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
struct InstallManifest {
    games: Vec<AppEntry>,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchInfo {
    pub id: u64,
    pub app: String,
    pub name: String,
    pub platform: String,
    pub issuer: i64,
    pub url: String,
    pub hash: u32,
    pub sig: String,
    #[serde(rename = "sig_hash")]
    pub sig_hash: u32,
    pub arch: String,
}

struct UIState {
    startup: bool,
    startup_text: String,
    prepare: bool,
    prepare_text: String,
    update: bool,
    update_text: String,
    launch: bool,
    launch_text: String,
    error_text: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // startup step

    // initalize user interface library
    let user_interface = UI::init().expect("UI library failed to initialize.");

    // find user preferences
    let mut manifest = InstallManifest::default();
    let mut entry = AppEntry::default();
    let mut manifest_found = false;
    if let Some(proj_dirs) = ProjectDirs::from("fm", "Orchestra FM", "AppLauncher") {
        let data_local_dir = proj_dirs.data_local_dir();

        if data_local_dir.join("install.manifest").exists().eq(&false) {
            MessageAlert {
                title: "Game not found",
                text: "It appears that this game, Unnamed SDVX Clone, is not installed or was not found. You will now be prompted to choose an install location.",
                typ: MessageType::Warning,
            }.show()?;

            let install_dir_dialog = OpenSingleDir { dir: None };
            if let Some(install_dir) = install_dir_dialog.show()? {
                entry = AppEntry {
                    name: String::from("unnamed-sdvx-clone"),
                    dir: install_dir,
                    patch: 0,
                };

                // create directories while we are at it
                fs::create_dir_all(data_local_dir).unwrap();
            } else {
                MessageAlert {
                    title: "No directory chosen",
                    text: "Required action was either cancelled or was invalid, exiting.",
                    typ: MessageType::Error,
                }
                .show()?;
                std::process::exit(2);
            }
        } else {
            manifest_found = true;
        }
    }
    //manifest.games.push(entry);

    // prepare user interface state
    let ui_state = Rc::new(RefCell::new(UIState {
        startup: true,
        startup_text: "Startup...                                                                                 OK".into(),
        prepare: true,
        prepare_text: "Prepare...                                                                                 OK".into(),
        update: false,
        update_text: "Update".into(),
        launch: false,
        launch_text: "Launch".into(),
        error_text: "".into(),
    }));

    // setup and organize controls
    let (main_vbox, startup_label, prepare_label, update_label, launch_label, error_label) = {
        let mut main_vbox = VerticalBox::new(&user_interface);
        let startup_label = Label::new(&user_interface, "");
        let prepare_label = Label::new(&user_interface, "");
        let update_label = Label::new(&user_interface, "");
        let launch_label = Label::new(&user_interface, "");
        let error_label = Label::new(&user_interface, "");

        main_vbox.append(
            &user_interface,
            startup_label.clone(),
            LayoutStrategy::Stretchy,
        );
        main_vbox.append(
            &user_interface,
            prepare_label.clone(),
            LayoutStrategy::Stretchy,
        );
        main_vbox.append(
            &user_interface,
            update_label.clone(),
            LayoutStrategy::Stretchy,
        );
        main_vbox.append(
            &user_interface,
            launch_label.clone(),
            LayoutStrategy::Stretchy,
        );
        main_vbox.append(
            &user_interface,
            error_label.clone(),
            LayoutStrategy::Stretchy,
        );

        (
            main_vbox,
            startup_label,
            prepare_label,
            update_label,
            launch_label,
            error_label,
        )
    };

    // connect controls to the main window
    let mut main_window = Window::new(
        &user_interface,
        "AppLauncher - Orchestra FM",
        300,
        300,
        WindowType::NoMenubar,
    );
    main_window.set_child(&user_interface, main_vbox);
    main_window.show(&user_interface);

    // spin up a helper thread
    let (send_state, recv_state) = unbounded();
    let helper_thread = thread::spawn(move || {
        defer_on_unwind! {
            send_state.send("An error has occured.".to_string());
        }
        // get required updates list
        send_state.send("Contacting Server...".to_string()).unwrap();

        let mut patch_resp_params: HashMap<String, String> = HashMap::new();
        patch_resp_params.insert("app".into(), "unnamed-sdvx-clone".into());
        patch_resp_params.insert("platform".into(), "win32".into());
        patch_resp_params.insert("version".into(), entry.patch.to_string());

        let patch_list_resp = HTTP_CLIENT
            .get("https://orchestra.fm/api/v0/patch")
            .form(&patch_resp_params)
            .send()
            .unwrap();

        if patch_list_resp.status().ne(&StatusCode::OK) {
            send_state
                .send("ERROR: Update server did not respond.".to_string())
                .unwrap();
            return;
        }
        let patch_list = patch_list_resp.json::<Vec<PatchInfo>>().unwrap();

        // iterate through patch list
        let total_tasks = patch_list.len() * 5;
        let mut i = 0;

        let notify_finished_download_task = |total_tasks: usize, i: &mut i32| {
            *i += 1;
            send_state
                .send(format!("Downloading File ({}/{})...", i, total_tasks))
                .unwrap();
        };

        let notify_finished_checksum_task = |total_tasks: usize, i: &mut i32| {
            *i += 1;
            send_state
                .send(format!("Comparing File Hashes ({}/{})...", i, total_tasks))
                .unwrap();
        };

        let notify_finished_applying_task = |total_tasks: usize, i: &mut i32| {
            *i += 1;
            send_state
                .send(format!("Applying ({}/{})...", i, total_tasks))
                .unwrap();
        };

        for patch in patch_list.iter() {
            // download patch file
            notify_finished_download_task(total_tasks, &mut i);

            let mut out_patch_file = fs::File::create("tmp-file.pwr").unwrap();
            defer! { fs::remove_file("tmp-file.pwr").expect(""); }
            let mut download_patch_resp = HTTP_CLIENT.get(&patch.url).send().expect("");
            io::copy(&mut download_patch_resp, &mut out_patch_file).expect("");

            // download signature file
            notify_finished_download_task(total_tasks, &mut i);

            let mut out_sig_file = fs::File::create("tmp-file.pwr.sig").unwrap();
            defer! { fs::remove_file("tmp-file.pwr.sig").expect(""); }
            let mut download_sig_resp = HTTP_CLIENT.get(&patch.sig).send().expect("");
            io::copy(&mut download_sig_resp, &mut out_sig_file).expect("");

            // comparing file checksum
            notify_finished_checksum_task(total_tasks, &mut i);

            let patch_file = fs::read("tmp-file.pwr").expect("");
            let patch_file_crc32c = crc32c::crc32c(patch_file.as_slice());

            if patch_file_crc32c.ne(&patch.hash) {
                println!("Downloaded: {}, Server: {}", patch_file_crc32c, patch.hash);
                send_state
                    .send("CRC32 Checksum on patch did not match.".into())
                    .unwrap();
                return;
            }

            // comparing file checksum
            notify_finished_checksum_task(total_tasks, &mut i);

            let sig_file = fs::read("tmp-file.pwr.sig").expect("");
            let sig_file_crc32c = crc32c::crc32c(sig_file.as_slice());

            if sig_file_crc32c.ne(&patch.sig_hash) {
                println!(
                    "Downloaded: {}, Server: {}",
                    sig_file_crc32c, patch.sig_hash
                );
                send_state
                    .send("CRC32 Checksum on signature did not match.".into())
                    .unwrap();
                return;
            }

            // apply patch to directory
            notify_finished_applying_task(total_tasks, &mut i);

            fs::create_dir("butler-workingdir").expect("");
            defer! { fs::remove_dir_all("butler-workingdir").expect("") }
            let cmd_output = process::Command::new("tools/butler")
                .args(&[
                    "apply",
                    "--staging-dir",
                    "butler-workingdir",
                    "tmp-file.pwr",
                    entry.dir.to_str().expect(""),
                    "--signature",
                    "tmp-file.pwr.sig",
                ])
                .output()
                .expect("");
            println!(
                "stdout: {}",
                std::str::from_utf8(cmd_output.stdout.as_slice()).expect("")
            );
            println!(
                "stderr: {}",
                std::str::from_utf8(cmd_output.stderr.as_slice()).expect("")
            );

            entry.patch = patch.id as u16;
        }
        send_state.send("allok".into()).unwrap();
        manifest.games.push(entry);

        // persist manifest to disk
        if manifest_found.eq(&false) {
            if let Some(proj_dirs) = ProjectDirs::from("fm", "Orchestra FM", "AppLauncher") {
                use std::io::prelude::*;
                let data_local_dir = proj_dirs.data_local_dir();

                let mut manifest_file =
                    fs::File::create(data_local_dir.join("install.manifest")).unwrap();
                let serialized_manifest = toml::to_string(&manifest).unwrap();
                manifest_file
                    .write_all(serialized_manifest.as_bytes())
                    .unwrap();
                manifest_file.sync_all().unwrap();
            }
        } else {
            panic!("now what");
        }
    });

    // main event loop
    let mut current_operation = String::from("Waiting For Tasks...");
    let mut event_loop = user_interface.event_loop();
    event_loop.on_tick(&user_interface, {
        // update labels
        let user_interface = user_interface.clone();
        let mut startup_label = startup_label.clone();
        let mut prepare_label = prepare_label.clone();
        let mut update_label = update_label.clone();
        let mut launch_label = launch_label.clone();
        let mut error_label = error_label.clone();

        move || {
            let mut ui_state = ui_state.borrow_mut();

            startup_label.set_text(&user_interface, &format!("{}", ui_state.startup_text));
            prepare_label.set_text(&user_interface, &format!("{}", ui_state.prepare_text));
            update_label.set_text(&user_interface, &format!("{}", ui_state.update_text));
            launch_label.set_text(&user_interface, &format!("{}", ui_state.launch_text));
            error_label.set_text(&user_interface, &format!("{}", current_operation));

            if ui_state.update.eq(&false) {
                match recv_state.try_recv() {
                    Err(e) => {
                        if e.is_disconnected().eq(&true) {
                            ui_state.update = true;
                        }
                    }
                    Ok(performing_operation) => {
                        if performing_operation.eq("allok") {
                            current_operation = "Launching requested application.".into();
                            ui_state.update_text = "Update...                                                                                  OK".into();
                        } else if performing_operation.contains("error") {
                            ui_state.update_text = "Update...                                                                                  FAIL".into();
                        } else {
                            current_operation = performing_operation;
                        }
                    }
                }
            }

        }
    });

    event_loop.run_delay(&user_interface, 16);

    Ok(())
}
