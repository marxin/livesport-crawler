use chrono::LocalResult::Single;
use chrono::{DateTime, Datelike, Local, TimeZone};
use fantoccini::Client;
use fantoccini::{wd::Capabilities, ClientBuilder, Locator};
use std::sync::mpsc::channel;
use std::{
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};
use tracing::{info, warn};
use url::Url;

const DRIVER_PORT: u16 = 9515;
const TEAM_NAME: &str = "Sparta Praha";
const TEAM_URL: &str = "https://www.livesport.cz/tym/sparta-praha/zcG9U7N6/";
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[allow(dead_code)]
#[derive(Debug)]
struct GameResult {
    my_team: String,
    my_team_score: u32,
    opponent_team: String,
    opponent_team_score: u32,
    event_date: DateTime<Local>,
}

fn start_driver() -> anyhow::Result<Child> {
    let driver = Command::new("chromedriver")
        .arg(format!("--port={}", DRIVER_PORT))
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .spawn()?;

    thread::sleep(Duration::from_millis(300));
    Ok(driver)
}

async fn get_score(client: &mut Client) -> anyhow::Result<GameResult> {
    let base_url = Url::parse(TEAM_URL)?;
    client.goto(base_url.join("vysledky")?.as_str()).await?;

    let match_rows = client.find_all(Locator::Css(".event__match")).await?;
    let last_match_row = &match_rows
        .first()
        .ok_or(anyhow::anyhow!("could not find .event__match element"))?;

    let home_team = last_match_row
        .find(Locator::Css(".event__participant--home"))
        .await?
        .text()
        .await?;

    let away_team = last_match_row
        .find(Locator::Css(".event__participant--away"))
        .await?
        .text()
        .await?;

    let home_score = last_match_row
        .find(Locator::Css(".event__score--home"))
        .await?
        .text()
        .await?
        .parse::<u32>()?;

    let away_score = last_match_row
        .find(Locator::Css(".event__score--away"))
        .await?
        .text()
        .await?
        .parse::<u32>()?;

    let event_time = last_match_row
        .find(Locator::Css(".event__time"))
        .await?
        .text()
        .await?;
    let event_time = event_time
        .split('\n')
        .next()
        .ok_or(anyhow::anyhow!(".event__time is empty"))?
        .to_string();

    // TODO: we assume the last match happened in the same year!
    let event_time_parts = event_time
        .split(['.', ' ', ':'])
        .filter(|s| !s.is_empty())
        .map(|x| x.parse::<u32>())
        .collect::<Result<Vec<_>, _>>()?;

    let now = Local::now();
    // TODO: Properly map an error if the result is equal to LocalResult::Ambiguous!
    let event_date = now.timezone().with_ymd_and_hms(
        now.year(),
        event_time_parts[1],
        event_time_parts[0],
        event_time_parts[2],
        event_time_parts[3],
        0,
    );
    let Single(event_date) = event_date else {
        anyhow::bail!("ambigous or none date: {event_date:?}");
    };

    let latest_match = if home_team.starts_with(TEAM_NAME) {
        GameResult {
            my_team: home_team,
            my_team_score: home_score,
            opponent_team: away_team,
            opponent_team_score: away_score,
            event_date,
        }
    } else {
        GameResult {
            my_team: away_team,
            my_team_score: away_score,
            opponent_team: home_team,
            opponent_team_score: home_score,
            event_date,
        }
    };

    Ok(latest_match)
}

// let's set up the sequence of steps we want the browser to take
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let (tx, rx) = channel();

    ctrlc::set_handler(move || {
        info!("termination handler called");
        tx.send(()).expect("Could not send signal on channel.");
    })
    .expect("Error setting Ctrl-C handler");

    let mut driver = start_driver()?;

    let cap: Capabilities =
        serde_json::from_str(r#"{"goog:chromeOptions":{"args":["--headless"]}}"#).unwrap();

    let mut c = ClientBuilder::rustls()?
        .capabilities(cap)
        .connect(&format!("http://localhost:{DRIVER_PORT}"))
        .await
        .expect("failed to connect to WebDriver");

    loop {
        match get_score(&mut c).await {
            Ok(latest_match) => {
                info!("latest match = {latest_match:?}");
            }
            Err(error) => {
                warn!("got error: {error}");
            }
        }

        if rx.try_recv().is_ok() {
            info!("exitting the main loop");
            break;
        }

        tokio::time::sleep(REFRESH_INTERVAL).await;
    }

    driver.kill().unwrap();

    c.close().await?;

    Ok(())
}
