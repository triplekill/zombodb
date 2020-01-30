use crate::elasticsearch::{BulkRequestCommand, BulkRequestError, Elasticsearch};
use pgx::*;
use serde_json::{json, Value};
use std::any::Any;
use std::collections::HashMap;
use std::io::{Error, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

const BULK_FILTER_PATH: &str = "errors,items.*.error";

pub(crate) struct Handler {
    threads: Vec<JoinHandle<usize>>,
}

struct BulkReceiver {
    receiver: crossbeam::channel::Receiver<BulkRequestCommand>,
    bytes_out: usize,
    docs_out: Arc<AtomicUsize>,
    backlog: Vec<u8>,
}

impl std::io::Read for BulkReceiver {
    fn read(&mut self, mut buf: &mut [u8]) -> Result<usize, Error> {
        let mut bytes = &mut self.backlog;

        if self.docs_out.load(Ordering::SeqCst) < 10_000 && self.bytes_out < 8 * 1024 * 1024 {
            // we haven't exceeded the max _bulk docs limit

            for command in self.receiver.iter() {
                self.docs_out.fetch_add(1, Ordering::SeqCst);

                // build json of this entire command and store in self.bytes
                match command {
                    BulkRequestCommand::Insert {
                        ctid,
                        cmin: _,
                        cmax: _,
                        xmin: _,
                        xmax: _,
                        doc,
                    } => {
                        serde_json::to_writer(
                            &mut bytes,
                            &json! {
                                {"index": {"_id": item_pointer_to_u64(ctid) } }
                            },
                        )
                        .expect("failed to serialize index line");
                        bytes.push(b'\n');

                        serde_json::to_writer(&mut bytes, &doc).expect("failed to serialize doc");
                        bytes.push(b'\n');
                    }
                    BulkRequestCommand::Update { .. } => panic!("unsupported"),
                    BulkRequestCommand::DeleteByXmin { .. } => panic!("unsupported"),
                    BulkRequestCommand::DeleteByXmax { .. } => panic!("unsupported"),
                    BulkRequestCommand::Interrupt => panic!("unsupported"),
                    BulkRequestCommand::Done => panic!("unsupported"),
                }

                break;
            }
        }

        let amt = buf.write(&bytes)?;
        if amt > 0 {
            // move our bytes forward the amount we wrote above
            let (_, right) = bytes.split_at(amt);
            self.backlog = Vec::from(right);
            self.bytes_out += amt;
        }

        Ok(amt)
    }
}

impl From<BulkReceiver> for reqwest::Body {
    fn from(reader: BulkReceiver) -> Self {
        reqwest::Body::new(reader)
    }
}

impl Handler {
    pub(crate) fn run(
        elasticsearch: Elasticsearch,
        concurrency: usize,
        bulk_receiver: crossbeam::channel::Receiver<BulkRequestCommand>,
        _error_sender: crossbeam::channel::Sender<BulkRequestError>,
    ) -> Self {
        let mut threads = Vec::new();
        for i in 0..concurrency {
            let es = elasticsearch.clone();
            let rx = bulk_receiver.clone();

            let jh = std::thread::spawn(move || {
                let mut total_docs_out = 0;

                loop {
                    let docs_out = Arc::new(AtomicUsize::new(0));

                    let rx = rx.clone();
                    let reader = BulkReceiver {
                        receiver: rx.clone(),
                        bytes_out: 0,
                        docs_out: docs_out.clone(),
                        backlog: Vec::new(),
                    };

                    let url = &format!(
                        "{}{}/_bulk?filter_path={}",
                        es.url, es.index_name, BULK_FILTER_PATH
                    );
                    let client = reqwest::Client::new();
                    let response = client
                        .post(url)
                        .header("content-type", "application/json")
                        .body(reader)
                        .send();

                    let docs_out = docs_out.load(Ordering::SeqCst);
                    total_docs_out += docs_out;

                    eprintln!("thread#{}: docs_out={}", i, docs_out);
                    match response {
                        // we got a valid response from ES
                        Ok(mut response) => {
                            // quick check on the status code
                            let code = response.status().as_u16();
                            if code < 200 || (code >= 300 && code != 404) {
                                let mut resp_string = String::new();
                                response
                                    .read_to_string(&mut resp_string)
                                    .expect("unable to convert HTTP response to a string");
                                panic!("{}", resp_string)
                            } else if code != 200 {
                                let mut resp_string = String::new();
                                response
                                    .read_to_string(&mut resp_string)
                                    .expect("unable to convert HTTP response to a string");

                                match serde_json::from_str::<HashMap<String, Value>>(&resp_string) {
                                    // got a valid json response
                                    Ok(response) => {
                                        // does it contain a general error?
                                        match *response.get("error").unwrap_or(&Value::Bool(false))
                                        {
                                            Value::Bool(b) if b == false => { /* we're all good */ }
                                            _ => panic!("{}", resp_string),
                                        }

                                        // does it contain errors related to the docs we indexed?
                                        match *response.get("errors").unwrap_or(&Value::Bool(false))
                                        {
                                            Value::Bool(b) if b == false => { /* we're all good */ }
                                            _ => panic!("{}", resp_string),
                                        }
                                    }

                                    // got a response that wasn't json, so just panic with it
                                    Err(_) => panic!("{}", resp_string),
                                }
                            }
                        }

                        // this is likely a general reqwest/network communication error
                        Err(e) => panic!("{:?}", e),
                    }

                    if docs_out == 0 || rx.is_empty() {
                        break;
                    }
                }

                eprintln!("thread#{}: total_docs_out={}", i, total_docs_out);
                total_docs_out
            });

            threads.push(jh)
        }

        Handler { threads }
    }

    pub(crate) fn wait_for_completion(self) -> Result<usize, BulkRequestError> {
        let mut cnt = 0;

        for jh in self.threads {
            match jh.join() {
                Ok(many) => {
                    cnt += many;
                }
                Err(e) => panic!("Got an error joining on a thread: {}", downcast_err(e)),
            }
        }

        Ok(cnt)
    }

    pub(crate) fn terminate(&mut self) {}
}

fn downcast_err(e: Box<dyn Any + Send>) -> String {
    if let Some(s) = e.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = e.downcast_ref::<String>() {
        s.to_string()
    } else {
        // not a type we understand, so use a generic string
        "Box<Any>".to_string()
    }
}
