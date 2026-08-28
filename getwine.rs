use std::{
    env,
    fs,
    io::{self, Read, Write},
    path::Path,
    process::{Command, Stdio, exit},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const ORANGE: &str = "\x1b[38;5;208m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

struct UserContext {
    uid: String,
    username: String,
    home: String,
    session_environment: Vec<(String, String)>,
}

impl UserContext {
    fn from_pkexec() -> Result<Self, String> {
        let uid = env::var("PKEXEC_UID")
            .map_err(|_| "GetWine must be authorized through Polkit by a desktop user.".to_string())?;
        if uid == "0" || !uid.chars().all(|character| character.is_ascii_digit()) {
            return Err("Polkit did not provide a valid non-root desktop user.".to_string());
        }

        let account = Command::new("getent")
            .args(["passwd", &uid])
            .output()
            .map_err(|error| format!("Could not resolve the desktop user: {error}"))?;
        if !account.status.success() {
            return Err("Could not resolve the desktop user account.".to_string());
        }

        let account = String::from_utf8_lossy(&account.stdout);
        let fields = account.trim().split(':').collect::<Vec<_>>();
        if fields.len() < 7 || fields[0].is_empty() || fields[5].is_empty() {
            return Err("The desktop user account record is incomplete.".to_string());
        }

        let username = fields[0].to_string();
        let home = fields[5].to_string();
        let wine_prefix = format!("{home}/wine");
        let runtime_directory = format!("/run/user/{uid}");
        if !Path::new(&runtime_directory).is_dir() {
            return Err("The desktop user's runtime session is not available.".to_string());
        }

        let mut context = Self {
            uid,
            username,
            home,
            session_environment: vec![
                ("XDG_RUNTIME_DIR".to_string(), runtime_directory.clone()),
                (
                    "DBUS_SESSION_BUS_ADDRESS".to_string(),
                    format!("unix:path={runtime_directory}/bus"),
                ),
                ("WINEPREFIX".to_string(), wine_prefix),
                ("WINEARCH".to_string(), "win64".to_string()),
            ],
        };
        context.load_desktop_environment();
        Ok(context)
    }

    fn load_desktop_environment(&mut self) {
        let runtime_directory = format!("/run/user/{}", self.uid);
        let output = Command::new("runuser")
            .args(["-u", &self.username, "--", "systemctl", "--user", "show-environment"])
            .env("XDG_RUNTIME_DIR", &runtime_directory)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={runtime_directory}/bus"),
            )
            .output();

        let allowed = [
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "XAUTHORITY",
            "XDG_CURRENT_DESKTOP",
            "KDE_FULL_SESSION",
            "LANG",
            "LC_ALL",
        ];
        if let Ok(output) = output {
            if output.status.success() {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    if let Some((key, value)) = line.split_once('=') {
                        if allowed.contains(&key) {
                            self.session_environment
                                .push((key.to_string(), value.to_string()));
                        }
                    }
                }
            }
        }

        if !self
            .session_environment
            .iter()
            .any(|(key, _)| key == "WAYLAND_DISPLAY")
        {
            if let Ok(entries) = fs::read_dir(&runtime_directory) {
                if let Some(display) = entries
                    .flatten()
                    .filter_map(|entry| entry.file_name().into_string().ok())
                    .find(|name| name.starts_with("wayland-") && !name.ends_with(".lock"))
                {
                    self.session_environment
                        .push(("WAYLAND_DISPLAY".to_string(), display));
                }
            }
        }
    }

    fn command(&self, program: &str, args: &[&str]) -> Command {
        let mut command = Command::new("runuser");
        command
            .args(["-u", &self.username, "--", program])
            .args(args)
            .env("HOME", &self.home)
            .env("USER", &self.username)
            .env("LOGNAME", &self.username);
        for (key, value) in &self.session_environment {
            command.env(key, value);
        }
        command
    }

    fn run_shell(&self, command: &str) -> bool {
        self.command("sh", &["-c", command])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

fn run_root(cmd: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn effective_uid() -> Option<u32> {
    let output = Command::new("id").arg("-u").output().ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn elevate_with_polkit() {
    if effective_uid() == Some(0) {
        return;
    }

    let executable = env::current_exe().unwrap_or_else(|error| {
        eprintln!("Could not locate GetWine: {error}");
        exit(1);
    });
    let status = Command::new("pkexec")
        .arg(executable)
        .args(env::args().skip(1))
        .status()
        .unwrap_or_else(|error| {
            eprintln!("Could not request Polkit authorization: {error}");
            exit(1);
        });
    exit(status.code().unwrap_or(1));
}

fn networkmanager_reports_network() -> bool {
    let output = match Command::new("nmcli")
        .args(["-t", "-f", "STATE", "general"])
        .env("LC_ALL", "C")
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return false,
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|state| state.trim().starts_with("connected"))
}

enum ProgressEvent {
    DownloadStarted,
    Percentage(u8),
}

fn percentage_from_line(line: &str) -> Option<u8> {
    let percent_position = line.find('%')?;
    let digits = line[..percent_position]
        .chars()
        .rev()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();

    digits.parse::<u8>().ok().filter(|percentage| *percentage <= 100)
}

fn forward_progress<R: Read + Send + 'static>(reader: R, sender: mpsc::Sender<ProgressEvent>) {
    thread::spawn(move || {
        let mut line = Vec::new();

        for byte in reader.bytes().flatten() {
            if byte == b'\r' || byte == b'\n' {
                if !line.is_empty() {
                    let output = String::from_utf8_lossy(&line);
                    if output.trim_start().starts_with("Downloading http") {
                        let _ = sender.send(ProgressEvent::DownloadStarted);
                    }
                    if let Some(percentage) = percentage_from_line(&output) {
                        let _ = sender.send(ProgressEvent::Percentage(percentage));
                    }
                    line.clear();
                }
            } else if line.len() < 16_384 {
                line.push(byte);
            }
        }
    });
}

fn draw_component_progress(
    label: &str,
    component_number: usize,
    component_total: usize,
    component_percentage: usize,
) {
    const WIDTH: usize = 30;
    let filled = component_percentage * WIDTH / 100;
    let bar = format!("{}{}", "#".repeat(filled), "-".repeat(WIDTH - filled));

    print!(
        "\r  {component_number}/{component_total} {label} [{bar}] {component_percentage:>3}%\x1b[K"
    );
    let _ = io::stdout().flush();
}

fn draw_component_working(
    label: &str,
    component_number: usize,
    component_total: usize,
) {
    print!("\r  {component_number}/{component_total} {label}: installing...\x1b[K");
    let _ = io::stdout().flush();
}

fn run_with_progress(
    user: &UserContext,
    label: &str,
    program: &str,
    args: &[&str],
    debug: bool,
    component_number: usize,
    component_total: usize,
    expected_downloads: usize,
) -> bool {
    if debug {
        println!("  {component_number}/{component_total} {label}");
        return user
            .command(program, args)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
    }

    let mut command = user.command(program, args);
    let mut child = match command
        .env("WINETRICKS_DOWNLOADER", "wget")
        .env("WGETRC", "/dev/null")
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => {
            println!(
                "  {component_number}/{component_total} {label}: {RED}FAILED{RESET}"
            );
            return false;
        }
    };

    let (sender, receiver) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        forward_progress(stdout, sender.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        forward_progress(stderr, sender.clone());
    }
    drop(sender);

    let expected_downloads = expected_downloads.max(1);
    let mut download_number = 0usize;
    let mut component_percentage = 0usize;
    let started = Instant::now();
    let mut showing_installation_status = true;
    let mut last_percentage_at: Option<Instant> = None;
    draw_component_working(label, component_number, component_total);

    loop {
        while let Ok(event) = receiver.try_recv() {
            let candidate = match event {
                ProgressEvent::DownloadStarted => {
                    download_number = (download_number + 1).min(expected_downloads);
                    (download_number.saturating_sub(1) * 100) / expected_downloads
                }
                ProgressEvent::Percentage(percentage) => {
                    last_percentage_at = Some(Instant::now());
                    if download_number == 0 {
                        download_number = 1;
                    }
                    ((download_number - 1) * 100 + percentage as usize) / expected_downloads
                }
            }
            .min(99);

            if candidate > component_percentage {
                component_percentage = candidate;
                showing_installation_status = false;
                draw_component_progress(
                    label,
                    component_number,
                    component_total,
                    component_percentage,
                );
            }
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    println!(
                        "\r  ✅ {component_number}/{component_total} finished: {label}\x1b[K",
                    );
                } else {
                    println!(
                        "\r  {component_number}/{component_total} {label}: {RED}FAILED{RESET}\x1b[K"
                    );
                }
                return status.success();
            }
            Ok(None) => {
                let download_is_idle = last_percentage_at
                    .map(|updated| updated.elapsed() >= Duration::from_secs(2))
                    .unwrap_or(true);
                if download_is_idle && !showing_installation_status && started.elapsed().as_secs() > 0 {
                    showing_installation_status = true;
                    draw_component_working(label, component_number, component_total);
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => {
                let _ = child.kill();
                println!(
                    "\r  {component_number}/{component_total} {label}: {RED}FAILED{RESET}\x1b[K"
                );
                return false;
            }
        }
    }
}

fn pause_exit(code: i32) -> ! {
    println!("Press Enter to close GetWine.");
    let mut buf = String::new();
    let _ = io::stdin().read_line(&mut buf);
    exit(code);
}

fn ask_yes_no(prompt: &str, default_yes: bool) -> bool {
    let default = if default_yes { "[Y/n]" } else { "[y/N]" };

    loop {
        print!("{} {} ", prompt, default);
        let _ = io::stdout().flush();

        let mut input = String::new();
        let _ = io::stdin().read_line(&mut input);

        let answer = input.trim().to_lowercase();

        if answer.is_empty() {
            return default_yes;
        }

        match answer.as_str() {
            "y" | "yes" | "yeah" => return true,
            "n" | "no" => return false,
            _ => println!("Please answer yes or no."),
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let debug = match args.get(1) {
        Some(arg) if arg == "-debug" => true,
        Some(_) => {
            println!("wrong argument!");
            return;
        }
        None => false,
    };

    elevate_with_polkit();

    let user = UserContext::from_pkexec().unwrap_or_else(|error| {
        eprintln!("{ORANGE}ERROR: {error}{RESET}\n");
        pause_exit(1);
    });

    let wine_prefix = format!("{}/wine", user.home);
    let dot_wine = format!("{}/.wine", user.home);

    run_root("clear");

    if !networkmanager_reports_network() {
        eprintln!("{ORANGE}ERROR: No network connection was reported.{RESET}");
        eprintln!("GetWine cannot continue without network connectivity.");
        eprintln!("Connect to a network, then launch GetWine again.\n");
        pause_exit(1);
    }

    if debug {
        println!("-debug");
    }

    println!("getwine - Wine installer for Fluff Linux\n");

    println!(
        "{ORANGE}WARNING! The installer will remove any existing version of wine{RESET}\n"
    );

    println!(
        "{ORANGE}NOTE: Internet connectivity is required to complete setup.{RESET}\n"
    );

    println!(
        "{ORANGE}THIRD-PARTY SOFTWARE NOTICE:{RESET}\n\
         GetWine installs DXVK, VKD3D-Proton, Microsoft Core Fonts, Microsoft\n\
         XACT, and the Microsoft Direct3D compiler. Winetricks downloads these\n\
         required components during setup. Microsoft components are proprietary and\n\
         subject to Microsoft's license terms. Fluff Linux does not distribute\n\
         or license these components. Continuing starts the complete installation.\n"
    );

    if !ask_yes_no("Continue?", false) {
        exit(1);
    }

    run_root("pacman -Sy");

    println!("\n🔄 Removing any existing Wine packages...");

    run_root("pacman -Rdd --noconfirm wine-staging 2>/dev/null");
    run_root("pacman -Rdd --noconfirm wine-mono 2>/dev/null");
    run_root("pacman -Rdd --noconfirm wine-gecko 2>/dev/null");
    run_root("pacman -Rdd --noconfirm winetricks 2>/dev/null");
    run_root("pacman -Rdd --noconfirm wine 2>/dev/null");
    run_root("pacman -Rdd --noconfirm dosbox 2>/dev/null");
    run_root("pacman -Rdd --noconfirm wine-wow64 2>/dev/null");
    run_root("pacman -Rdd --noconfirm wine-staging-wow64 2>/dev/null");
    run_root("pacman -Rdd --noconfirm wine-stable 2>/dev/null");

    if fs::metadata(&dot_wine).is_ok() {
        println!("🗑️ Removing old ~/.wine prefix...");
        user.run_shell(&format!("rm -rf '{}'", dot_wine));
    }

    if fs::metadata(&wine_prefix).is_ok() {
        println!("🗑️ Removing old ~/wine prefix...");
        user.run_shell(&format!("rm -rf '{}'", wine_prefix));
    }

    println!("Installing Wine...");

    run_root("pacman -S --needed --noconfirm wine wine-mono wine-gecko winetricks dosbox");

    if run_root("pacman -Q wine >/dev/null 2>&1") {
        println!("✅ Wine installed successfully!");
    } else {
        println!("❌ wine installation failed. please check for errors.");
        pause_exit(1);
    }

    println!("Initializing Wine prefix...");

    if debug {
        user.run_shell("wineboot --init --update");
    } else {
        user.run_shell("wineboot --init --update >/dev/null 2>&1");
    }

    println!("\nInstalling required Wine components...");

    let components = [
        ("Microsoft Core Fonts", "corefonts", 10),
        ("DXVK", "dxvk", 1),
        ("VKD3D-Proton", "vkd3d", 1),
        ("Microsoft XACT (64-bit)", "xact_x64", 1),
        ("Microsoft Direct3D Compiler 43", "d3dcompiler_43", 1),
    ];

    let component_total = components.len();
    let mut failed_components = Vec::new();
    for (index, (label, verb, expected_downloads)) in components.iter().enumerate() {
        if !run_with_progress(
            &user,
            label,
            "winetricks",
            &["-q", verb],
            debug,
            index + 1,
            component_total,
            *expected_downloads,
        ) {
            failed_components.push(*label);
        }
    }

    let installed_components = component_total - failed_components.len();
    println!(
        "{installed_components}/{component_total} required Wine components installed."
    );

    if fs::metadata(&dot_wine).is_err() {
        user.run_shell(&format!("ln -s '{}' '{}'", wine_prefix, dot_wine));
    }

    user.run_shell("mkdir -p ~/.local/share/applications");
    user.run_shell("cp /etc/getwine/wine.desktop ~/.local/share/applications");
    user.run_shell("chmod +x ~/.local/share/applications/wine.desktop");
    user.run_shell("update-desktop-database ~/.local/share/applications");

    user.run_shell("xdg-mime default wine.desktop application/x-ms-dos-executable >/dev/null 2>&1");
    user.run_shell("xdg-mime default wine.desktop application/x-msi >/dev/null 2>&1");
    user.run_shell("xdg-mime default wine.desktop application/vnd.microsoft.portable-executable >/dev/null 2>&1");
    user.run_shell("xdg-mime default wine.desktop application/x-ms-shortcut >/dev/null 2>&1");
    user.run_shell("xdg-mime default wine.desktop application/x-bat >/dev/null 2>&1");
    user.run_shell("xdg-mime default wine.desktop application/x-mswinurl >/dev/null 2>&1");

    user.run_shell("kbuildsycoca6 --noincremental");

    if failed_components.is_empty() {
        run_root("rm -f /usr/share/applications/getwine.desktop >/dev/null 2>&1");
        user.run_shell("rm -f ~/.local/share/applications/getwine.desktop >/dev/null 2>&1");
        println!("✅ Wine setup complete!");
    } else {
        println!(
            "\n{ORANGE}WARNING: Some Winetricks components failed to download.{RESET}"
        );
        for component in &failed_components {
            println!("  - {component}");
        }
        println!("The GetWine launcher was kept so you can retry later.");
        println!("Run GetWine with -debug to inspect Winetricks errors.");
    }

    pause_exit(0);
}

#[cfg(test)]
mod tests {
    use super::percentage_from_line;

    #[test]
    fn parses_wget_progress() {
        assert_eq!(
            percentage_from_line(" 1024K .......... ..........  42%  2.1M 3s"),
            Some(42)
        );
        assert_eq!(percentage_from_line("archive.tar.zst 100% 25.0M 0s"), Some(100));
    }

    #[test]
    fn ignores_lines_without_valid_progress() {
        assert_eq!(percentage_from_line("Downloading archive"), None);
        assert_eq!(percentage_from_line("Result: 150%"), None);
    }
}
