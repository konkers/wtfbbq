use anyhow::Result;
use controller::CommandSender;
use rocket::{get, response::Debug, routes, State};
use rocket_contrib::json::Json;
use rocket_contrib::serve::StaticFiles;
use std::sync::Arc;
use tokio::time::Duration;
use tokio::{self, sync::Mutex};

mod controller;

#[get("/status")]
async fn status(
    status: State<'_, Arc<Mutex<controller::Status>>>,
) -> std::result::Result<Json<controller::Status>, Debug<anyhow::Error>> {
    let status = status.lock().await.clone();

    Ok(Json(status))
}

#[get("/params/setpoint/<value>")]
async fn setpoint(
    value: f32,
    cmd_tx: State<'_, CommandSender>,
) -> std::result::Result<Json<f32>, Debug<anyhow::Error>> {
    cmd_tx.set_set_point(value).await?;
    Ok(Json(value))
}

#[get("/params/p/<value>")]
async fn p_gain(
    value: f32,
    cmd_tx: State<'_, CommandSender>,
) -> std::result::Result<Json<f32>, Debug<anyhow::Error>> {
    cmd_tx.set_p_gain(value).await?;
    Ok(Json(value))
}

#[get("/params/i/<value>")]
async fn i_gain(
    value: f32,
    cmd_tx: State<'_, CommandSender>,
) -> std::result::Result<Json<f32>, Debug<anyhow::Error>> {
    cmd_tx.set_i_gain(value).await?;
    Ok(Json(value))
}

#[get("/params/d/<value>")]
async fn d_gain(
    value: f32,
    cmd_tx: State<'_, CommandSender>,
) -> std::result::Result<Json<f32>, Debug<anyhow::Error>> {
    cmd_tx.set_d_gain(value).await?;
    Ok(Json(value))
}

#[get("/params/tc/<index>")]
async fn tc_index(
    index: usize,
    cmd_tx: State<'_, CommandSender>,
) -> std::result::Result<Json<usize>, Debug<anyhow::Error>> {
    cmd_tx.set_tc_index(index).await?;
    Ok(Json(index))
}

#[tokio::main]
pub async fn main() -> Result<()> {
    let config = controller::Config {
        p: 4.0,
        i: 0.025,
        d: 0.0,
        set_point: 0.0,
        tc_index: 0,
        period: Duration::from_millis(10000),
    };
    let controller = controller::Controller::new(config)?;
    let (cmd_tx, mut status_rx) = controller.start()?;

    let status = Arc::new(Mutex::new(controller::Status::default()));
    let web = rocket::build()
        .manage(status.clone())
        .manage(cmd_tx)
        .mount("/", StaticFiles::from("./static"))
        .mount(
            "/",
            routes![status, setpoint, p_gain, i_gain, d_gain, tc_index],
        )
        .launch();
    tokio::pin!(web);

    println!("web ui at http://127.0.0.1:8000");

    loop {
        tokio::select! {
            new_status = status_rx.recv_status() => {
                let mut status = status.lock().await;
                *status = new_status.unwrap();
            }
            res = &mut web => {
                println!("web res: {:?}", res);
            }
        }
    }
}
