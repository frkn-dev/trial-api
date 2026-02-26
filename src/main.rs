use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use warp::Filter;

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    sync::{Arc, Mutex},
};

use lettre::{
    transport::smtp::authentication::Credentials, AsyncSmtpTransport, AsyncTransport, Message,
    Tokio1Executor,
};
use reqwest::Client;

/* ================= CONFIG ================= */

const CSV_FILE: &str = "trials.csv";
const DEFAULT_DAYS: i64 = 1;
const DEFAULT_ENV: &str = "dev";

const PROTOS: [&str; 4] = [
    "VlessTcpReality",
    "VlessGrpcReality",
    "VlessXhttpReality",
    "Hysteria2",
];

/* ================= MODELS ================= */

#[derive(Debug, Deserialize)]
struct TrialRequest {
    email: Option<String>,
    telegram: Option<String>,
    source: Option<TrialSource>,
    env: Option<String>,
}

impl TrialRequest {
    fn validate(&self) -> bool {
        if self.email.is_none() && self.source.is_none() {
            false
        } else if self.email.is_some() && self.source.is_some() {
            false
        } else {
            true
        }
    }
}

#[derive(Debug, Serialize)]
struct TrialResponse {
    status: String,
    message: String,
    sub_id: Option<String>,
}

use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct ApiResponse<T> {
    pub status: u16,
    pub message: String,
    pub response: T,
}

/* ===== SUBSCRIPTION ===== */

#[derive(Debug, Deserialize)]
pub struct SubscriptionResponse {
    pub status: u16,
    pub message: String,
    pub response: SubscriptionInstance,
}

#[derive(Debug, Deserialize)]
pub struct SubscriptionInstance {
    #[serde(rename = "Subscription")]
    pub subscription: Subscription,
}

#[derive(Debug, Deserialize)]
pub struct SubscriptionEnvelope {
    pub id: Uuid,
    pub instance: SubscriptionInstance,
}

#[derive(Debug, Deserialize)]
pub struct Subscription {
    pub id: Uuid,
    pub expires_at: String,
    pub referred_by: Option<String>,
    pub refer_code: String,
    pub bonus_days: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
    pub is_deleted: bool,
}

/* ===== CONNECTION ===== */

#[derive(Debug, Deserialize)]
pub struct ConnectionResponse {
    pub id: Uuid,
    pub instance: ConnectionInstance,
}

#[derive(Debug, Deserialize)]
pub struct ConnectionInstance {
    #[serde(rename = "Connection")]
    pub connection: Connection,
}

#[derive(Debug, Deserialize)]
pub struct Connection {
    pub env: String,
    pub proto: ProtoWrapper,
    pub stat: ConnectionStat,
    pub subscription_id: Uuid,
    pub created_at: String,
    pub modified_at: String,
    pub expired_at: Option<String>,
    pub is_deleted: bool,
    pub node_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub enum XrayProto {
    VlessXhttpReality,
    VlessGrpcReality,
    VlessTcpReality,
    Hysteria2,
}

#[derive(Debug, Deserialize)]
pub struct Hysteria2Proto {
    pub token: uuid::Uuid,
}

#[derive(Debug, Deserialize)]
pub enum ProtoWrapper {
    #[serde(rename = "Xray")]
    Xray(XrayProto),
    #[serde(rename = "Hysteria2")]
    Hysteria2(Hysteria2Proto),
}

#[derive(Debug, Deserialize)]
pub struct ConnectionStat {
    pub downlink: u64,
    pub uplink: u64,
    pub online: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum TrialSource {
    Mobile,
    Site,
}

impl fmt::Display for TrialSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrialSource::Mobile => write!(f, "trial-mobile"),
            TrialSource::Site => write!(f, "trial-site"),
        }
    }
}

type Store = Arc<Mutex<HashMap<String, DateTime<Utc>>>>;
type HttpClient = Client;

/* ================= MAIN ================= */

#[tokio::main]
async fn main() {
    let store: Store = Arc::new(Mutex::new(load_trials()));
    let store_filter = warp::any().map(move || store.clone());

    let http = Client::new();
    let http_filter = warp::any().map(move || http.clone());

    let cors = warp::cors()
        .allow_any_origin()
        .allow_methods(vec!["POST", "OPTIONS"])
        .allow_headers(vec!["Content-Type"]);

    let route = warp::post()
        .and(warp::path("trial"))
        .and(warp::body::json())
        .and(store_filter)
        .and(http_filter)
        .and_then(handle_trial)
        .with(cors);

    println!("🚀 Trial service on 127.0.0.1:3030");
    warp::serve(route).run(([127, 0, 0, 1], 3030)).await;
}

/* ================= HANDLER ================= */

async fn handle_trial(
    req: TrialRequest,
    store: Store,
    http: HttpClient,
) -> Result<impl warp::Reply, warp::Rejection> {
    if !req.validate() {
        return Ok(warp::reply::json(&TrialResponse {
            status: "error".into(),
            message: "Trial request is not valid".into(),
            sub_id: None,
        }));
    }

    let email = req.email.clone();

    /* ================= ATOMIC TRIAL CHECK ================= */

    if let Some(ref email) = email {
        let mut guard = store.lock().unwrap();

        // insert returns old value if existed
        if guard.insert(email.clone(), Utc::now()).is_some() {
            return Ok(warp::reply::json(&TrialResponse {
                status: "error".into(),
                message: "Trial already requested".into(),
                sub_id: None,
            }));
        }
    }

    let now = Utc::now();

    /* ================= CREATE SUB ================= */

    let referred_by = req.source.unwrap_or(TrialSource::Site);
    let env = req.env.as_deref().unwrap_or("DEFAULT_ENV");

    let sub_id = match create_subscription(&http, env, DEFAULT_DAYS, &referred_by).await {
        Ok(id) => id,
        Err(e) => {
            eprintln!("❌ subscription error: {}", e);
            return Ok(warp::reply::json(&TrialResponse {
                status: "error".into(),
                message: "Failed to create subscription".into(),
                sub_id: None,
            }));
        }
    };

    /* ================= CONNECTIONS PARALLEL ================= */

    use futures::future::join_all;

    let futures = PROTOS.iter().map(|proto| {
        let http = http.clone();
        let sub_id = sub_id;

        async move {
            if *proto == "Hysteria2" {
                let token = Uuid::new_v4();
                create_connection(&http, DEFAULT_ENV, proto, &sub_id, &Some(token)).await
            } else {
                create_connection(&http, DEFAULT_ENV, proto, &sub_id, &None).await
            }
        }
    });

    for r in join_all(futures).await {
        if let Err(e) = r {
            eprintln!("❌ connection error: {}", e);
        }
    }

    /* ================= SAVE + EMAIL ================= */

    if let Some(email) = email {
        if let Err(e) = save_trial(&email, req.telegram.as_deref(), &sub_id, DEFAULT_ENV, &now) {
            eprintln!("csv error: {}", e);
        }

        if let Err(e) = send_email(&email, &sub_id).await {
            eprintln!("📧 email error: {}", e);
        }

        return Ok(warp::reply::json(&TrialResponse {
            status: "ok".into(),
            message: "Trial activated. Check your email.".into(),
            sub_id: Some(sub_id.to_string()),
        }));
    }

    Ok(warp::reply::json(&TrialResponse {
        status: "ok".into(),
        message: "Trial activated.".into(),
        sub_id: Some(sub_id.to_string()),
    }))
}

/* ================= FRKN API ================= */

fn auth_headers(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    req.header(
        "Authorization",
        format!("Bearer {}", std::env::var("FRKN_API_TOKEN").unwrap()),
    )
    .header("Accept", "application/json")
    .header("Content-Type", "application/json")
}

async fn create_subscription(
    http: &Client,
    env: &str,
    days: i64,
    referred_by: &TrialSource,
) -> anyhow::Result<Uuid> {
    let host = std::env::var("FRKN_HOST")?;

    let res = auth_headers(
        http.post(format!("{}/subscription", host))
            .json(&serde_json::json!({
                "env": env,
                "days": days,
                "referred_by": referred_by.to_string()
            })),
    )
    .send()
    .await?;

    let status = res.status();
    let text = res.text().await?;

    println!("STATUS = {}", status);
    println!("BODY = {}", text);

    let parsed: ApiResponse<SubscriptionEnvelope> = serde_json::from_str(&text)?;
    Ok(parsed.response.id)
}

pub async fn create_connection(
    http: &Client,
    env: &str,
    proto: &str,
    sub_id: &Uuid,
    token: &Option<Uuid>,
) -> anyhow::Result<String> {
    let host = std::env::var("FRKN_HOST")?;

    let res = if let Some(token) = token {
        auth_headers(
            http.post(format!("{}/connection", host))
                .json(&serde_json::json!({
                    "env": env,
                    "proto": proto,
                    "subscription_id": sub_id,
                    "token": token,
                })),
        )
        .send()
        .await?
    } else {
        auth_headers(
            http.post(format!("{}/connection", host))
                .json(&serde_json::json!({
                    "env": env,
                    "proto": proto,
                    "subscription_id": sub_id
                })),
        )
        .send()
        .await?
    };

    let status = res.status();
    let text = res.text().await?;

    println!("Connection resp: {}", text);

    if text.is_empty() {
        anyhow::bail!("empty connection response, status = {}", status);
    }

    let parsed: ApiResponse<ConnectionResponse> = serde_json::from_str(&text)?;

    Ok(parsed.response.id.to_string())
}

/* ================= EMAIL ================= */

async fn send_email(to: &str, sub_id: &Uuid) -> Result<(), Box<dyn std::error::Error>> {
    let user = std::env::var("GMAIL_USER")?;
    let pass = std::env::var("GMAIL_APP_PASSWORD")?;
    let host = std::env::var("FRKN_HOST")?;

    // HTML письмо
    let html_body = format!(
        r#"
<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<title>FRKN VPN Trial</title>
<style>
    body {{
        font-family: Arial, sans-serif;
        background-color: #f4f4f4;
        margin: 0;
        padding: 0;
    }}
    .container {{
        width: 100%;
        max-width: 600px;
        margin: 0 auto;
        background-color: #ffffff;
        padding: 20px;
        border-radius: 12px;
        box-shadow: 0 4px 12px rgba(0,0,0,0.1);
    }}
    .header {{
        text-align: center;
        margin-bottom: 20px;
    }}
    .logo {{
        max-width: 150px;
    }}
    h1 {{
        color: #1d4ed8; /* фирменный синий */
        font-size: 24px;
    }}
    p {{
        color: #374151;
        font-size: 16px;
        line-height: 1.5;
    }}
    .button {{
        display: inline-block;
        padding: 12px 24px;
        background-color: #1d4ed8;
        color: #ffffff;
        text-decoration: none;
        border-radius: 8px;
        margin: 20px 0;
        font-weight: bold;
    }}
    .footer {{
        font-size: 12px;
        color: #9ca3af;
        text-align: center;
        margin-top: 20px;
    }}
</style>
</head>
<body>
<div class="container">
    <div class="header">
        
        <h1>Твой триал активирован!</h1>
    </div>
    <p>Привет!</p>
    <p>Твой триал для <strong>FRKN</strong> успешно активирован 🎉</p>
    <p>Информация по подписке:</p>
    <p>
        <strong>ID:</strong> {sub_id}<br/>
        <strong>Ссылка:</strong> <a href="{host}/sub/info?id={sub_id}">{host}/sub/info?id={sub_id}</a>
    </p>
    <a href="{host}/sub/info?id={sub_id}"

   style="
       display: inline-block;
       padding: 12px 24px;
       background-color: #1d4ed8;
       color: #ffffff !important;
       text-decoration: none;
       border-radius: 8px;
       font-weight: bold;
   ">
   Перейти к подписке
</a>



    <p>Подписывайся на наш Telegram: <a href="https://t.me/frkn_org">@frkn_org</a></p>
    <div class="footer"> <a href="https://t.me/frkn_support">Поддержка</a></p> <br>
        Vive la résistance!<br/>
        © 2026 FRKN
    </div>
</div>
</body>
</html>
"#,
        host = host,
        sub_id = sub_id
    );

    let msg = Message::builder()
        .from(format!("FRKN <{}>", user).parse()?)
        .to(to.parse()?)
        .subject("FRKN VPN Trial 🚀")
        .header(lettre::message::header::ContentType::TEXT_HTML)
        .body(html_body)?;

    let mailer = AsyncSmtpTransport::<Tokio1Executor>::relay("smtp.gmail.com")?
        .credentials(Credentials::new(user.clone(), pass))
        .build();

    mailer.send(msg).await?;
    Ok(())
}

/* ================= CSV ================= */

fn save_trial(
    email: &str,
    tg: Option<&str>,
    sub_id: &Uuid,
    env: &str,
    time: &DateTime<Utc>,
) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(CSV_FILE)?;

    writeln!(
        file,
        "{},{},{},{},{}",
        time.to_rfc3339(),
        email,
        tg.unwrap_or(""),
        sub_id,
        env
    )?;

    Ok(())
}

fn load_trials() -> HashMap<String, DateTime<Utc>> {
    let mut map = HashMap::new();

    if let Ok(file) = File::open(CSV_FILE) {
        for line in BufReader::new(file).lines().flatten() {
            let parts: Vec<_> = line.split(',').collect();
            if parts.len() >= 2 {
                if let Ok(ts) = parts[0].parse::<DateTime<Utc>>() {
                    map.insert(parts[1].to_string(), ts);
                }
            }
        }
    }
    map
}
