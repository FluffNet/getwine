use std::{
    env,
    fs,
    io::{self, Read, Write},
    process::{Child, Command, Stdio, exit},
    sync::mpsc,
    thread,
    time::Duration,
};

const ORANGE: &str = "\x1b[38;5;208m";
const RESET: &str = "\x1b[0m";

struct SudoSession {
    keepalive: Child,
}

impl SudoSession {
    fn start() -> Result<Self, String> {
        println!("Administrator authorization is required to install Wine.");
        let status = Command::new("sudo")
            .arg("-v")
            .status()
            .map_err(|error| format!("Could not request administrator authorization: {error}"))?;

        if !status.success() {
            return Err("Administrator authorization was not granted.".to_string());
        }

        let cached = Command::new("sudo")
            .args(["-n", "true"])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !cached {
            return Err("The current sudo policy does not allow GetWine to maintain one authorization session.".to_string());
        }

        let keepalive = Command::new("sh")
            .args([
                "-c",
                "while kill -0 \"$GETWINE_PARENT_PID\" 2>/dev/null; do sudo -n -v >/dev/null 2>&1 || exit 0; sleep 30; done",
            ])
            .env("GETWINE_PARENT_PID", std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("Could not maintain administrator authorization: {error}"))?;

        Ok(Self { keepalive })
    }
}

impl Drop for SudoSession {
    fn drop(&mut self) {
        let _ = self.keepalive.kill();
        let _ = self.keepalive.wait();
    }
}

fn run(cmd: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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
    let overall_percentage =
        (((component_number - 1) * 100 + component_percentage) / component_total).min(99);
    let filled = overall_percentage * WIDTH / 100;
    let bar = format!("{}{}", "#".repeat(filled), "-".repeat(WIDTH - filled));

    print!(
        "\r  [{bar}] {overall_percentage:>3}% overall | {component_number}/{component_total} {label}: {component_percentage:>3}%\x1b[K"
    );
    let _ = io::stdout().flush();
}

fn run_with_progress(
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
        return Command::new(program)
            .args(args)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
    }

    let mut child = match Command::new(program)
        .args(args)
        .env("WINETRICKS_DOWNLOADER", "wget")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => {
            println!("  {component_number}/{component_total} failed: {label}");
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
    draw_component_progress(
        label,
        component_number,
        component_total,
        component_percentage,
    );

    loop {
        while let Ok(event) = receiver.try_recv() {
            let candidate = match event {
                ProgressEvent::DownloadStarted => {
                    download_number = (download_number + 1).min(expected_downloads);
                    (download_number.saturating_sub(1) * 100) / expected_downloads
                }
                ProgressEvent::Percentage(percentage) => {
                    if download_number == 0 {
                        download_number = 1;
                    }
                    ((download_number - 1) * 100 + percentage as usize) / expected_downloads
                }
            }
            .min(99);

            if candidate > component_percentage {
                component_percentage = candidate;
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
                    let overall_percentage = component_number * 100 / component_total;
                    let filled = overall_percentage * 30 / 100;
                    println!(
                        "\r  [{}{}] {overall_percentage:>3}% overall | {component_number}/{component_total} finished: {label}\x1b[K",
                        "#".repeat(filled),
                        "-".repeat(30 - filled),
                    );
                } else {
                    println!(
                        "\r  [failed] {component_number}/{component_total} {label}\x1b[K"
                    );
                }
                return status.success();
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(_) => {
                let _ = child.kill();
                println!("\r  [failed] {component_number}/{component_total} {label}\x1b[K");
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

    let home = env::var("HOME").unwrap_or_else(|_| {
        eprintln!("Could not detect home directory.");
        exit(1);
    });

    let wine_prefix = format!("{}/wine", home);
    let dot_wine = format!("{}/.wine", home);

    run("clear");

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

    let sudo_session = match SudoSession::start() {
        Ok(session) => session,
        Err(error) => {
            eprintln!("{ORANGE}ERROR: {error}{RESET}\n");
            pause_exit(1);
        }
    };

    run("sudo -n pacman -Sy");

    println!("\n🔄 Removing any existing Wine packages...");

    run("sudo -n pacman -Rdd --noconfirm wine-staging 2>/dev/null");
    run("sudo -n pacman -Rdd --noconfirm wine-mono 2>/dev/null");
    run("sudo -n pacman -Rdd --noconfirm wine-gecko 2>/dev/null");
    run("sudo -n pacman -Rdd --noconfirm winetricks 2>/dev/null");
    run("sudo -n pacman -Rdd --noconfirm wine 2>/dev/null");
    run("sudo -n pacman -Rdd --noconfirm dosbox 2>/dev/null");
    run("yay -Rdd --noconfirm wine-wow64 2>/dev/null");
    run("yay -Rdd --noconfirm wine-staging-wow64 2>/dev/null");
    run("yay -Rdd --noconfirm wine-stable 2>/dev/null");

    if fs::metadata(&dot_wine).is_ok() {
        println!("🗑️ Removing old ~/.wine prefix...");
        run(&format!("rm -rf '{}'", dot_wine));
    }

    if fs::metadata(&wine_prefix).is_ok() {
        println!("🗑️ Removing old ~/wine prefix...");
        run(&format!("rm -rf '{}'", wine_prefix));
    }

    println!("Installing Wine...");

    run("sudo -n pacman -S --needed --noconfirm wine wine-mono wine-gecko winetricks dosbox");

    if run("pacman -Q wine >/dev/null 2>&1") {
        println!("✅ Wine installed successfully!");
    } else {
        println!("❌ wine installation failed. please check for errors.");
        pause_exit(1);
    }

    println!("Initializing Wine prefix...");

    unsafe {
        env::set_var("WINEPREFIX", &wine_prefix);
        env::set_var("WINEARCH", "win64");
    }

    if debug {
        run("wineboot --init --update");
    } else {
        run("wineboot --init --update >/dev/null 2>&1");
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
    for (index, (label, verb, expected_downloads)) in components.iter().enumerate() {
        if !run_with_progress(
            label,
            "winetricks",
            &["-q", verb],
            debug,
            index + 1,
            component_total,
            *expected_downloads,
        ) {
            eprintln!("❌ Failed to install {label}.");
            eprintln!("Run GetWine with -debug to see Winetricks output.");
            pause_exit(1);
        }
    }

    if fs::metadata(&dot_wine).is_err() {
        run(&format!("ln -s '{}' '{}'", wine_prefix, dot_wine));
    }

    run("mkdir -p ~/.local/share/applications");
    run("cp /etc/getwine/wine.desktop ~/.local/share/applications");
    run("chmod +x ~/.local/share/applications/wine.desktop");
    run("update-desktop-database ~/.local/share/applications");

    run("xdg-mime default wine.desktop application/x-ms-dos-executable >/dev/null 2>&1");
    run("xdg-mime default wine.desktop application/x-msi >/dev/null 2>&1");
    run("xdg-mime default wine.desktop application/vnd.microsoft.portable-executable >/dev/null 2>&1");
    run("xdg-mime default wine.desktop application/x-ms-shortcut >/dev/null 2>&1");
    run("xdg-mime default wine.desktop application/x-bat >/dev/null 2>&1");
    run("xdg-mime default wine.desktop application/x-mswinurl >/dev/null 2>&1");

    run("kbuildsycoca6 --noincremental");

    run("sudo -n rm -f /usr/share/applications/getwine.desktop >/dev/null 2>&1");
    run("rm -f ~/.local/share/applications/getwine.desktop >/dev/null 2>&1");

    drop(sudo_session);

    println!("✅ Wine setup complete!");

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
