use std::{
    env,
    fs,
    io::{self, Write},
    process::{Command, Stdio, exit},
    thread,
    time::Duration,
};

const ORANGE: &str = "\x1b[38;5;208m";
const RESET: &str = "\x1b[0m";

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

fn run_with_activity_bar(label: &str, program: &str, args: &[&str], debug: bool) -> bool {
    if debug {
        println!("  {label}");
        return Command::new(program)
            .args(args)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
    }

    let mut child = match Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => {
            println!("  [failed] {label}");
            return false;
        }
    };

    const WIDTH: usize = 24;
    let mut position = 0usize;
    let mut moving_right = true;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                print!("\r  [{}] {label}", if status.success() { "=".repeat(WIDTH) } else { "!".repeat(WIDTH) });
                println!(" {}", if status.success() { "done" } else { "failed" });
                let _ = io::stdout().flush();
                return status.success();
            }
            Ok(None) => {
                let mut bar = vec![' '; WIDTH];
                bar[position] = '█';
                let bar: String = bar.into_iter().collect();
                print!("\r  [{bar}] {label}");
                let _ = io::stdout().flush();

                if moving_right {
                    if position + 1 == WIDTH {
                        moving_right = false;
                        position = position.saturating_sub(1);
                    } else {
                        position += 1;
                    }
                } else if position == 0 {
                    moving_right = true;
                    position += 1;
                } else {
                    position -= 1;
                }

                thread::sleep(Duration::from_millis(100));
            }
            Err(_) => {
                let _ = child.kill();
                println!("\r  [{}] {label} failed", "!".repeat(WIDTH));
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
        "{ORANGE}NOTE: downloads still require an internet connection to complete setup!{RESET}\n"
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

    run("sudo pacman -Sy");

    println!("\n🔄 Removing any existing Wine packages...");

    run("sudo pacman -Rdd --noconfirm wine-staging 2>/dev/null");
    run("sudo pacman -Rdd --noconfirm wine-mono 2>/dev/null");
    run("sudo pacman -Rdd --noconfirm wine-gecko 2>/dev/null");
    run("sudo pacman -Rdd --noconfirm winetricks 2>/dev/null");
    run("sudo pacman -Rdd --noconfirm wine 2>/dev/null");
    run("sudo pacman -Rdd --noconfirm dosbox 2>/dev/null");
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

    run("sudo pacman -S --needed --noconfirm wine wine-mono wine-gecko winetricks dosbox");

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
        ("Microsoft Core Fonts", "corefonts"),
        ("DXVK", "dxvk"),
        ("VKD3D-Proton", "vkd3d"),
        ("Microsoft XACT (64-bit)", "xact_x64"),
        ("Microsoft Direct3D Compiler 43", "d3dcompiler_43"),
    ];

    for (label, verb) in components {
        if !run_with_activity_bar(label, "winetricks", &["-q", verb], debug) {
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

    run("sudo rm /usr/share/applications/getwine.desktop >/dev/null 2>&1");
    run("sudo rm ~/.local/share/applications/getwine.desktop >/dev/null 2>&1");

    println!("✅ Wine setup complete!");

    pause_exit(0);
}
