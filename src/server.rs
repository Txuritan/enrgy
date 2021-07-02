use crate::service::Service;

use {
    self::pool::ThreadPool,
    crate::{app::BuiltApp, http::HttpError, App, HttpRequest, HttpResponse},
    std::{
        collections::BTreeMap,
        io::{self, Read as _, Write as _},
        net::{Shutdown, SocketAddr, TcpListener, TcpStream},
        sync::{atomic::AtomicBool, Arc},
        thread::{self, JoinHandle},
    },
};

enum ServerError {
    Http(HttpError),
    Io(io::Error),
    Utf8(std::string::FromUtf8Error),
}

impl From<HttpError> for ServerError {
    fn from(err: HttpError) -> Self {
        ServerError::Http(err)
    }
}

impl From<io::Error> for ServerError {
    fn from(err: io::Error) -> Self {
        ServerError::Io(err)
    }
}

impl From<std::string::FromUtf8Error> for ServerError {
    fn from(err: std::string::FromUtf8Error) -> Self {
        ServerError::Utf8(err)
    }
}

struct Unbound;

pub struct HttpServer<Addr> {
    close: Arc<AtomicBool>,

    workers: Vec<JoinHandle<()>>,

    addr: Addr,

    app: Arc<BuiltApp>,
}

impl HttpServer<Unbound> {
    pub fn new(app: App) -> Self {
        Self {
            close: Arc::new(AtomicBool::new(false)),
            workers: Vec::with_capacity(4),
            addr: Unbound,
            app: Arc::new(app.build()),
        }
    }

    pub fn bind<A>(self, addr: A) -> HttpServer<SocketAddr>
    where
        A: Into<SocketAddr>,
    {
        HttpServer {
            close: self.close,
            workers: self.workers,
            addr: addr.into(),
            app: self.app,
        }
    }
}

impl HttpServer<SocketAddr> {
    pub fn run(self) -> Result<(), io::Error> {
        let listener = TcpListener::bind(self.addr)?;

        let (pool, sender) = ThreadPool::new(4, |data| {
            if let Err(_) = Self::thread_handle(data) {
                if let Err(err) = data.1.write_all(&HttpResponse::bad_request().into_bytes()) {
                    // TODO: log the error here
                    data.1.shutdown(Shutdown::Both);
                }
            }
        });

        thread::spawn(move || {
            while let Ok((stream, addr)) = listener.accept() {
                if sender.send((Arc::clone(&self.app), stream, addr)).is_err() {
                    break;
                }
            }
        });

        pool.join();

        Ok(())
    }

    fn thread_handle(
        (app, mut stream, addr): (Arc<BuiltApp>, TcpStream, SocketAddr),
    ) -> Result<(), ServerError> {
        let bytes = Self::read_stream(&mut stream)?;

        let (header, body) = if let Some(i) = bytes
            .windows(4)
            .position(|window| window == &b"\r\n\r\n"[..])
        {
            let (header, body) = bytes.split_at(i + 2);

            (Vec::from(header), Vec::from(&body[2..]))
        } else {
            (bytes, vec![])
        };

        let header = String::from_utf8(header)?;

        let header_data = HttpRequest::parse_header(&header)?;

        let (service, parameters) = app
            .tree
            .get(&header_data.method)
            .and_then(|tree| tree.find(&header_data.url))
            .map(|(service, parameters)| {
                (
                    Arc::clone(service),
                    parameters
                        .into_iter()
                        .map(|(key, value)| (key.to_string(), value.to_string()))
                        .collect::<BTreeMap<_, _>>(),
                )
            })
            .unwrap_or_else(|| (app.not_found.clone(), BTreeMap::new()));

        let mut request =
            HttpRequest::from_parts(header_data, body, parameters, Arc::clone(&app.data));

        for middleware in &*app.middleware {
            middleware.before(&mut request);
        }

        let response = service.call(&mut request)?;

        for middleware in &*app.middleware {
            middleware.after(&request, &response);
        }

        stream.write_all(&response.into_bytes())?;

        Ok(())
    }

    fn read_stream(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
        const BUFFER_SIZE: usize = 512;
        const MAX_BYTES: usize = 1028 * 8;

        let mut data = Vec::with_capacity(512);

        let mut amount_read = 0;
        let mut read_buf = [0; BUFFER_SIZE];

        loop {
            let read = stream.read(&mut read_buf)?;

            if read == 0 {
                break;
            }

            amount_read += read;

            data.extend_from_slice(&read_buf[..read]);

            read_buf = [0; BUFFER_SIZE];

            if amount_read >= MAX_BYTES {
                break;
            }
        }

        Ok(data)
    }
}

mod pool {
    use std::{
        marker::PhantomData,
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc::{self, Receiver, RecvTimeoutError, Sender},
            Arc, Mutex,
        },
        thread::{self, JoinHandle},
        time::Duration,
    };

    pub struct ThreadPool<Data>
    where
        Data: Send + Sync + 'static,
    {
        close: Arc<AtomicBool>,
        workers: Vec<Worker<Data>>,
    }

    impl<Data> ThreadPool<Data>
    where
        Data: Send + Sync + 'static,
    {
        pub fn new<F>(size: usize, handler: F) -> (Self, Sender<Data>)
        where
            F: Fn(Data) + Clone + Send + Sync + 'static,
        {
            let close = Arc::new(AtomicBool::new(false));

            let (sender, receiver) = mpsc::channel();

            let receiver = Arc::new(Mutex::new(receiver));

            let workers = (0..size)
                .into_iter()
                .map(|id| {
                    Worker::new(
                        id,
                        Arc::clone(&close),
                        Arc::clone(&receiver),
                        handler.clone(),
                    )
                })
                .collect();

            (Self { close, workers }, sender)
        }

        pub fn join(self) {
            self.close.store(true, Ordering::Relaxed);

            for worker in self.workers {
                worker.join()
            }
        }
    }

    struct Worker<Data>
    where
        Data: Send + Sync + 'static,
    {
        id: usize,
        thread: JoinHandle<()>,
        _data: PhantomData<Data>,
    }

    impl<Data> Worker<Data>
    where
        Data: Send + Sync + 'static,
    {
        fn new<F>(
            id: usize,
            close: Arc<AtomicBool>,
            receiver: Arc<Mutex<Receiver<Data>>>,
            handle: F,
        ) -> Self
        where
            F: Fn(Data) + Clone + Send + Sync + 'static,
        {
            let thread = thread::spawn(move || Self::inner(id, close, receiver, handle));

            Self {
                id,
                thread,
                _data: PhantomData,
            }
        }

        fn inner<F>(
            id: usize,
            close: Arc<AtomicBool>,
            receiver: Arc<Mutex<Receiver<Data>>>,
            handle: F,
        ) where
            F: Fn(Data) + Clone + Send + Sync + 'static,
        {
            loop {
                let received = {
                    let receiver = receiver.lock().unwrap();

                    receiver.recv_timeout(Duration::from_millis(100))
                };

                match received {
                    Ok(data) => handle(data),
                    Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        if close.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                }
            }
        }

        fn join(self) {
            self.thread.join().unwrap()
        }
    }
}
