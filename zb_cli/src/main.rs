use clap::{Parser, Subcommand};
use console::style;
use indicatif::{HumanBytes, MultiProgress, ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use zb_io::install::create_installer;
use zb_io::{InstallProgress, ProgressCallback};

#[derive(Parser)]
#[command(name = "zb")]
#[command(about = "Zerobrew - A fast Homebrew-compatible package installer")]
#[command(version)]
struct Cli {
    /// Root directory for zerobrew data
    #[arg(long, default_value = "/opt/zerobrew")]
    root: PathBuf,

    /// Prefix directory for linked binaries
    #[arg(long, default_value = "/opt/homebrew")]
    prefix: PathBuf,

    /// Number of parallel downloads
    #[arg(long, default_value = "8")]
    concurrency: usize,

    /// Homebrew Cellar path to reuse existing packages (set to empty to disable)
    #[arg(long, default_value = "/opt/homebrew/Cellar")]
    homebrew_cellar: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Install a formula
    Install {
        /// Formula name to install
        formula: String,

        /// Skip linking executables
        #[arg(long)]
        no_link: bool,
    },

    /// Uninstall a formula (or all formulas if no name given)
    Uninstall {
        /// Formula name to uninstall (omit to uninstall all)
        formula: Option<String>,
    },

    /// List installed formulas
    List,

    /// Show info about an installed formula
    Info {
        /// Formula name
        formula: String,
    },

    /// Garbage collect unreferenced store entries
    Gc,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if let Err(e) = run(cli).await {
        eprintln!("{} {}", style("error:").red().bold(), e);
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), zb_core::Error> {
    // Use homebrew cellar if it exists and path is non-empty
    let homebrew_cellar = if cli.homebrew_cellar.as_os_str().is_empty() {
        None
    } else if cli.homebrew_cellar.exists() {
        Some(cli.homebrew_cellar)
    } else {
        None
    };

    let mut installer = create_installer(&cli.root, &cli.prefix, cli.concurrency, homebrew_cellar)?;

    match cli.command {
        Commands::Install { formula, no_link } => {
            let start = Instant::now();
            println!(
                "{} Installing {}...",
                style("==>").cyan().bold(),
                style(&formula).bold()
            );

            let plan = installer.plan(&formula).await?;

            println!(
                "{} Resolving dependencies ({} packages)...",
                style("==>").cyan().bold(),
                plan.formulas.len()
            );
            for f in &plan.formulas {
                println!(
                    "    {} {}",
                    style(&f.name).green(),
                    style(&f.versions.stable).dim()
                );
            }

            // Set up progress display
            let multi = MultiProgress::new();
            let bars: Arc<Mutex<HashMap<String, ProgressBar>>> = Arc::new(Mutex::new(HashMap::new()));

            let download_style = ProgressStyle::default_bar()
                .template("    {prefix:<16} {bar:25.cyan/dim} {bytes:>10}/{total_bytes:<10} {eta:>6}")
                .unwrap()
                .progress_chars("━━╸");

            let spinner_style = ProgressStyle::default_spinner()
                .template("    {prefix:<16} {spinner:.cyan} {msg}")
                .unwrap()
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");

            let done_style = ProgressStyle::default_spinner()
                .template("    {prefix:<16} {msg}")
                .unwrap();

            println!(
                "{} Downloading and installing...",
                style("==>").cyan().bold()
            );

            let bars_clone = bars.clone();
            let multi_clone = multi.clone();
            let download_style_clone = download_style.clone();
            let spinner_style_clone = spinner_style.clone();
            let done_style_clone = done_style.clone();

            let progress_callback: Arc<ProgressCallback> = Arc::new(Box::new(move |event| {
                let mut bars = bars_clone.lock().unwrap();
                match event {
                    InstallProgress::DownloadStarted { name, total_bytes } => {
                        let pb = if let Some(total) = total_bytes {
                            let pb = multi_clone.add(ProgressBar::new(total));
                            pb.set_style(download_style_clone.clone());
                            pb
                        } else {
                            let pb = multi_clone.add(ProgressBar::new_spinner());
                            pb.set_style(spinner_style_clone.clone());
                            pb.set_message("downloading...");
                            pb.enable_steady_tick(std::time::Duration::from_millis(80));
                            pb
                        };
                        pb.set_prefix(name.clone());
                        bars.insert(name, pb);
                    }
                    InstallProgress::DownloadProgress {
                        name,
                        downloaded,
                        total_bytes,
                    } => {
                        if let Some(pb) = bars.get(&name) {
                            if total_bytes.is_some() {
                                pb.set_position(downloaded);
                            }
                        }
                    }
                    InstallProgress::DownloadCompleted { name, total_bytes } => {
                        if let Some(pb) = bars.get(&name) {
                            if total_bytes > 0 {
                                pb.set_position(total_bytes);
                            }
                            pb.set_style(spinner_style_clone.clone());
                            pb.set_message("unpacking...");
                            pb.enable_steady_tick(std::time::Duration::from_millis(80));
                        }
                    }
                    InstallProgress::UnpackStarted { name } => {
                        if let Some(pb) = bars.get(&name) {
                            pb.set_message("unpacking...");
                        }
                    }
                    InstallProgress::UnpackCompleted { name } => {
                        if let Some(pb) = bars.get(&name) {
                            pb.set_message("linking...");
                        }
                    }
                    InstallProgress::LinkStarted { name } => {
                        if let Some(pb) = bars.get(&name) {
                            pb.set_message("linking...");
                        }
                    }
                    InstallProgress::LinkCompleted { name } => {
                        if let Some(pb) = bars.get(&name) {
                            pb.set_style(done_style_clone.clone());
                            pb.set_message(format!("{} installed", style("✓").green()));
                            pb.finish();
                        }
                    }
                    InstallProgress::Skipped { name } => {
                        let pb = multi_clone.add(ProgressBar::new_spinner());
                        pb.set_style(done_style_clone.clone());
                        pb.set_prefix(name.clone());
                        pb.set_message(format!("{} skipped (in Homebrew)", style("○").dim()));
                        pb.finish();
                        bars.insert(name, pb);
                    }
                }
            }));

            let result = installer
                .execute_with_progress(plan, !no_link, Some(progress_callback))
                .await?;

            // Finish any remaining bars
            {
                let bars = bars.lock().unwrap();
                for (_, pb) in bars.iter() {
                    if !pb.is_finished() {
                        pb.finish();
                    }
                }
            }

            let elapsed = start.elapsed();
            println!();
            println!(
                "{} Installed {} packages in {:.2}s",
                style("==>").cyan().bold(),
                style(result.installed).green().bold(),
                elapsed.as_secs_f64()
            );
        }

        Commands::Uninstall { formula } => {
            match formula {
                Some(name) => {
                    println!(
                        "{} Uninstalling {}...",
                        style("==>").cyan().bold(),
                        style(&name).bold()
                    );
                    installer.uninstall(&name)?;
                    println!(
                        "{} Uninstalled {}",
                        style("==>").cyan().bold(),
                        style(&name).green()
                    );
                }
                None => {
                    let installed = installer.list_installed()?;
                    if installed.is_empty() {
                        println!("No formulas installed.");
                        return Ok(());
                    }

                    println!(
                        "{} Uninstalling {} packages...",
                        style("==>").cyan().bold(),
                        installed.len()
                    );

                    for keg in installed {
                        print!("    {} {}...", style("○").dim(), keg.name);
                        installer.uninstall(&keg.name)?;
                        println!(" {}", style("✓").green());
                    }

                    println!(
                        "{} Uninstalled all packages",
                        style("==>").cyan().bold()
                    );
                }
            }
        }

        Commands::List => {
            let installed = installer.list_installed()?;

            if installed.is_empty() {
                println!("No formulas installed.");
            } else {
                for keg in installed {
                    println!("{} {}", style(&keg.name).bold(), style(&keg.version).dim());
                }
            }
        }

        Commands::Info { formula } => {
            if let Some(keg) = installer.get_installed(&formula) {
                println!("{}       {}", style("Name:").dim(), style(&keg.name).bold());
                println!("{}    {}", style("Version:").dim(), keg.version);
                println!("{}  {}", style("Store key:").dim(), &keg.store_key[..12]);
                println!(
                    "{}  {}",
                    style("Installed:").dim(),
                    chrono_lite_format(keg.installed_at)
                );
            } else {
                println!("Formula '{}' is not installed.", formula);
            }
        }

        Commands::Gc => {
            println!(
                "{} Running garbage collection...",
                style("==>").cyan().bold()
            );
            let removed = installer.gc()?;

            if removed.is_empty() {
                println!("No unreferenced store entries to remove.");
            } else {
                for key in &removed {
                    println!("    {} Removed {}", style("✓").green(), &key[..12]);
                }
                println!(
                    "{} Removed {} store entries",
                    style("==>").cyan().bold(),
                    style(removed.len()).green().bold()
                );
            }
        }
    }

    Ok(())
}

fn chrono_lite_format(timestamp: i64) -> String {
    // Simple timestamp formatting without pulling in chrono
    use std::time::{Duration, UNIX_EPOCH};

    let dt = UNIX_EPOCH + Duration::from_secs(timestamp as u64);
    format!("{:?}", dt)
}
