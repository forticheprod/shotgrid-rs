use actix_multipart::Multipart;
use actix_web::{middleware, post, web, App, HttpResponse, HttpServer, Responder};
use futures::{StreamExt, TryStreamExt};
use serde::Deserialize;
use shotgrid_rs::{Client, Error};
use std::env;
use tokio::sync::mpsc::Receiver;
use futures::stream::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

#[derive(Clone, Debug)]
struct Settings {
    server: String,
    script_name: String,
    script_key: String,
}

impl Settings {
    pub(crate) fn sg(&self) -> Client {
        Client::new(
            self.server.clone(),
            Some(self.script_name.as_str()),
            Some(self.script_key.as_str()),
        )
        .expect("sg client")
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    dotenv::dotenv().ok();
    env_logger::init();

    let settings = Settings {
        server: env::var("SG_SERVER").expect("SG_SERVER"),
        script_name: env::var("SG_SCRIPT_NAME").expect("SG_SCRIPT_NAME"),
        script_key: env::var("SG_SCRIPT_KEY").expect("SG_SCRIPT_KEY"),
    };

    let http_host = env::var("HOST").unwrap_or_else(|_| String::from("0.0.0.0"));
    let http_port = env::var("PORT")
        .unwrap_or_else(|_| String::from("7878"))
        .parse::<u16>()
        .unwrap();

    let bind_addr = format!("{}:{}", http_host, http_port);

    log::info!("Starting up on {}.", &bind_addr);

    Ok(HttpServer::new(move || {
        App::new()
            .wrap(middleware::Logger::default())
            .data(settings.clone())
            .service(upload)
    })
    .bind(bind_addr)?
    .run()
    .await?)
}

#[derive(Deserialize, Debug)]
struct Query {
    entity_type: String,
    entity_id: i32,
    field_name: String,
}

#[post("/")]
async fn upload(
    query: web::Query<Query>,
    settings: web::Data<Settings>,
    mut payload: Multipart,
) -> impl Responder {
    if let Ok(Some(mut field)) = payload.try_next().await {
        // if content disposition is empty, something went wrong
        let content_disposition = field.content_disposition().unwrap();

        // if this is empty, then the shape of the payload is wrong
        let disposition_name = content_disposition.get_name().unwrap();

        match disposition_name {
            "files" => {
                // XXX: If there is no filename, shotgrid-rs can't infer the mime type
                let filename = match content_disposition.get_filename() {
                    Some(name) => name,
                    None => return HttpResponse::InternalServerError().body("Filename is missing"),
                };

                // At this point, we have a data source (the `field`) and a filename,
                // so we can "do the upload".
                let (handle, sender) = do_upload(
                    settings.sg(),
                    query.entity_type.clone(),
                    query.entity_id,
                    query.field_name.clone(),
                    filename.to_string(),
                );

                while let Some(chunk) = field.next().await {
                    let bytes = match chunk {
                        Err(_) => {
                            return HttpResponse::InternalServerError().body("Chunk read error.")
                        }
                        Ok(chunk) => chunk.to_vec(),
                    };

                    if let Err(_) = sender.send(Ok(bytes)).await {
                        log::error!("Failed to send chunk to channel.");
                        return HttpResponse::InternalServerError().body("Upload failed.");
                    }
                }

                // Close out the stream!
                //
                // If you don't do this, it's like reading a file and waiting
                // forever for the filesystem to give you the next chunk of
                // bytes. Dropping the sender is like the EOF.
                drop(sender);

                log::info!("All chunks sent to channel.");

                match handle.await.unwrap() {
                    Err(err) => {
                        log::error!("{:?}", err);
                    }
                    Ok(_) => {}
                }
            }
            _ => return HttpResponse::InternalServerError().body("Invalid disposition name"),
        }
    }

    HttpResponse::Ok().body("Upload done!")
}

type AnyResult<T> = std::result::Result<T, Box<dyn std::error::Error + Sync + Send + 'static>>;

/// Custom stream adapter for `tokio::sync::mpsc::Receiver`
struct ReceiverAsStream {
    rx: Receiver<AnyResult<Vec<u8>>>,
}

impl Stream for ReceiverAsStream {
    type Item = AnyResult<Vec<u8>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.rx.poll_recv(cx) {
            Poll::Ready(Some(result)) => Poll::Ready(Some(result)),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl ReceiverAsStream {
    fn new(rx: Receiver<AnyResult<Vec<u8>>>) -> Self {
        Self { rx }
    }
}

/// Spawn a future to actually do the upload.
///
/// Returns a handle for the background task, and a channel `Sender` used to
/// stream bytes into the upload request's body.
fn do_upload(
    sg: Client,
    entity_type: String,
    entity_id: i32,
    field_name: String,
    filename: String,
) -> (
    tokio::task::JoinHandle<AnyResult<()>>,
    tokio::sync::mpsc::Sender<AnyResult<Vec<u8>>>,
) {
    log::info!("Initializing upload task.");

    // Create a bounded MPSC channel with a capacity of 5
    let (tx, rx) = tokio::sync::mpsc::channel::<AnyResult<Vec<u8>>>(5);

    let handle = tokio::task::spawn_local(async move {
        log::info!("Upload task start.");
        let sess = sg.authenticate_script().await.unwrap();

        // Convert `rx` into a `Stream`
        let stream = ReceiverAsStream::new(rx);

        // Upload using the `Stream`
        sess.upload(&entity_type, entity_id, Some(&field_name), &filename)
            .send_stream(stream)
            .await
            .map_err(|e| {
                log::error!("{}", e);
                Error::Unexpected(format!("Upload failed: {:?}", e))
            })?;
        
        log::info!("Upload task end.");
        Ok(())
    });

    (handle, tx)
}