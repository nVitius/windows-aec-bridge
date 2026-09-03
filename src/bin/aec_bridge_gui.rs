#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() {
    if let Err(error) = try_main() {
        let _ = aec_bridge::startup::show_startup_error_message(&format!(
            "AEC Bridge could not start.\n\n{error}"
        ));
    }
}

fn try_main() -> eframe::Result {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    let login_startup =
        arguments.len() == 1 && arguments[0] == aec_bridge::startup::LOGIN_STARTUP_ARGUMENT;
    let instance = aec_bridge::startup::acquire_single_instance().map_err(|error| {
        eframe::Error::AppCreation(Box::new(std::io::Error::other(error.to_string())))
    })?;
    let _instance_guard = match instance {
        aec_bridge::startup::SingleInstance::Acquired(guard) => guard,
        aec_bridge::startup::SingleInstance::AlreadyRunning => {
            if !login_startup {
                aec_bridge::startup::show_already_running_message().map_err(|error| {
                    eframe::Error::AppCreation(Box::new(std::io::Error::other(error.to_string())))
                })?;
            }
            return Ok(());
        }
    };
    aec_bridge::gui::run(aec_bridge::gui::GuiLaunchOptions { login_startup })
}
