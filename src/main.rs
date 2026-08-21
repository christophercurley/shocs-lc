mod config;

use std::error::Error;

use config::Config;
use lifx::LifxClient;
use tokio::time::sleep;

const SOURCE_ID: u32 = 0x5348_4F43; // "SHOC"

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::from_env()?;

    println!("SHOCS Light Controller starting...");
    println!("LIFX bind address      : {}", config.bind_addr);
    println!("LIFX broadcast address : {}", config.lifx_broadcast_addr);
    println!(
        "Discovery interval     : {} seconds",
        config.discovery_interval.as_secs()
    );
    println!();

    let client = LifxClient::bind(
        config.bind_addr,
        config.lifx_broadcast_addr,
        SOURCE_ID,
    )
    .await?;

    loop {
        println!("==================================================");
        println!("Discovering LIFX devices...");

        match client.discover().await {
            Ok(devices) => {
                println!("Found {} device(s)", devices.len());

                let mut devices: Vec<_> = devices.into_values().collect();
                devices.sort_by_key(|device| device.id);

                for device in &devices {
                    match client.get_label(device).await {
                        Ok(label) => {
                            println!(
                                "  {:#018x} -> {} [{}]",
                                device.id, device.addr, label
                            );
                        }
                        Err(err) => {
                            println!(
                                "  {:#018x} -> {} [label unavailable: {}]",
                                device.id, device.addr, err
                            );
                        }
                    }
                }
            }
            Err(err) => {
                eprintln!("LIFX discovery failed: {err}");
            }
        }

        println!(
            "Next discovery in {} seconds.",
            config.discovery_interval.as_secs()
        );
        println!();

        sleep(config.discovery_interval).await;
    }
}
