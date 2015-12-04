#![allow(unused_imports)]

use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use mio::tcp::*;
use std::io::{self,Read,ErrorKind};
use mio::*;
use bytes::{Buf,ByteBuf,MutByteBuf};
use bytes::buf::MutBuf;
use std::collections::HashMap;
use std::error::Error;
use mio::util::Slab;
use std::net::SocketAddr;
use std::str::{FromStr, from_utf8};
use time::precise_time_s;
use rand::random;
use openssl::ssl::{SslContext, SslMethod, Ssl, NonblockingSslStream, ServerNameCallback, ServerNameCallbackData};
use openssl::ssl::error::NonblockingSslError;
use openssl::x509::X509FileType;

use parser::http11::{HttpState,parse_headers};

use messages::{Command,HttpFront};

type BackendToken = Token;
#[derive(Debug,Clone,PartialEq,Eq)]
pub enum ConnectionStatus {
  Initial,
  ClientConnected,
  Connected,
  ClientClosed,
  ServerClosed,
  Closed
}

#[derive(Debug)]
pub enum HttpProxyOrder {
  Command(Command),
  Stop
}

#[derive(Debug)]
pub enum ServerMessage {
  AddedHttpFront,
  RemovedHttpFront,
  AddedInstance,
  RemovedInstance,
  Stopped
}


struct Client {
  backend:        Option<TcpStream>,
  http_state:     HttpState,
  stream:         NonblockingSslStream<TcpStream>,
  front_buf:      Option<MutByteBuf>,
  back_buf:       Option<MutByteBuf>,
  token:          Option<Token>,
  backend_token:  Option<Token>,
  back_interest:  EventSet,
  front_interest: EventSet,
  status:         ConnectionStatus,
  rx_count:       usize,
  tx_count:       usize,
}

impl Client {
  fn new(stream: NonblockingSslStream<TcpStream>) -> Option<Client> {
    Some(Client {
      backend:        None,
      http_state:     HttpState::Initial,
      stream:         stream,
      front_buf:      Some(ByteBuf::mut_with_capacity(2048)),
      back_buf:       Some(ByteBuf::mut_with_capacity(2048)),
      token:          None,
      backend_token:  None,
      back_interest:  EventSet::all(),
      front_interest: EventSet::all(),
      status:         ConnectionStatus::Initial,
      rx_count:       0,
      tx_count:       0,
    })
  }

  pub fn set_front_token(&mut self, token: Token) {
    self.token         = Some(token);
  }

  pub fn set_back_token(&mut self, token: Token) {
    self.backend_token = Some(token);
  }

  pub fn set_tokens(&mut self, token: Token, backend: Token) {
    self.token         = Some(token);
    self.backend_token = Some(backend);
  }

  pub fn tokens(&self) -> Option<(Token,Token)> {
    if let Some(front) = self.token {
      if let Some(back) = self.backend_token {
        return Some((front, back))
      }
    }
    None
  }

  pub fn close(&self) {
  }

  // Forward content to client
  fn writable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    //println!("in writable()");
    if let Some(mut buf) = self.back_buf.take() {
      //println!("in writable 2: back_buf contains {} bytes", buf.remaining());
      let mut b = buf.flip();
      println!("writable back_buf({}): {}", b.remaining(), from_utf8((&b as &Buf).bytes()).unwrap());
      //if self.status == ConnectionStatus::ServerClosed && b.remaining() == 0 {
      if b.remaining() == 0 {
        self.back_buf = Some(b.flip());
        return Ok(());
      }

      //let mut b:MutByteBuf = buf.flip();
      //let sl:&mut[u8] = b.mut_bytes();
      //match self.sock.try_write_buf(&mut buf) {
      match self.stream.write((&b as &Buf).bytes()) {
        /*Ok(None) => {
          println!("client flushing buf; WOULDBLOCK");

          self.back_buf = Some(buf);
          self.front_interest.insert(EventSet::writable());
        },*/
        Ok(r) => {
          //FIXME what happens if not everything was written?
          //if let Some((front,back)) = self.tokens() {
          //  println!("FRONT [{}<-{}]: wrote {} bytes", front.as_usize(), back.as_usize(), r);
          //}
          b.advance(r);

          self.tx_count = self.tx_count + r;

          //self.front_interest.insert(EventSet::readable());
          self.front_interest.remove(EventSet::writable());
          self.back_interest.insert(EventSet::readable());
        },
        Err(NonblockingSslError::WantRead) => {
          self.front_interest.insert(EventSet::readable());
          println!("writable WantRead");
        },
        Err(NonblockingSslError::WantWrite) => {
          self.front_interest.insert(EventSet::writable());
          println!("writable WantWrite");
        }
        Err(e) => {
          panic!("not implemented; client err={:?}", e);
        }
      }
      self.back_buf = Some(b.flip());
    }
    if let Some((frontend_token,backend_token)) = self.tokens() {
      if let Some(ref sock) = self.backend {
        event_loop.reregister(sock, backend_token, self.back_interest, PollOpt::edge()).unwrap();
      }
      event_loop.reregister(self.stream.get_ref(), frontend_token, self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    }
    Ok(())
  }

  fn has_host(&self) -> bool {
    if let HttpState::HasHost(_, _, _) = self.http_state {
      true
    } else {
      false
    }
  }
  fn is_proxying(&self) -> bool {
    if let HttpState::Proxying(_, _) = self.http_state {
      true
    } else {
      false
    }
  }

  // Read content from the client
  fn readable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    //println!("in readable()");
    //println!("in readable(): front_mut_buf contains {} bytes", buf.remaining());

    if let Some(mut buf) = self.front_buf.take() {
      //let mut sl: &mut[u8] = buf.mut_bytes();
      match self.stream.read(unsafe { buf.mut_bytes() }) {
        /*Ok(None) => {
          println!("client flushing buf; WOULDBLOCK");

          self.back_buf = Some(buf);
          self.front_interest.insert(EventSet::writable());
        },*/
        Ok(r) => {
          println!("FRONT [{:?}]: read {} bytes", self.token, r);
          unsafe { buf.advance(r) };
          if self.is_proxying() {
            //if let Some((front,back)) = self.tokens() {
            //  println!("FRONT [{}->{}]: read {} bytes", front.as_usize(), back.as_usize(), r);
            //}
            self.front_interest.remove(EventSet::readable());
            self.back_interest.insert(EventSet::writable());
            if let Some((frontend_token,backend_token)) = self.tokens() {
              if let Some(ref sock) = self.backend {
                event_loop.reregister(sock, backend_token, EventSet::readable(), PollOpt::edge()).unwrap();
              }
              event_loop.reregister(self.stream.get_ref(), frontend_token, self.front_interest, PollOpt::edge() | PollOpt::oneshot());
            }
            self.rx_count = self.rx_count + r;
          } else {
            let state = parse_headers(&self.http_state, &buf.bytes());
            if let HttpState::Error(_) = state {
              self.http_state = state;
              panic!(" HTTP parsing error");
            }
            self.http_state = state;
            //println!("new state: {:?}", self.http_state);
            if self.has_host() {
              self.rx_count = buf.remaining();
              self.front_interest.remove(EventSet::readable());
              self.back_interest.insert(EventSet::writable());
              if let Some((frontend_token,backend_token)) = self.tokens() {
                if let Some(ref sock) = self.backend {
                  event_loop.reregister(sock, backend_token, EventSet::readable(), PollOpt::edge()).unwrap();
                }
                event_loop.reregister(self.stream.get_ref(), frontend_token, self.front_interest, PollOpt::edge() | PollOpt::oneshot());
              }
              //println!("is now proxying, front buf flipped");
            } else {
              self.front_interest.insert(EventSet::readable());
            }
          }
        },
        Err(NonblockingSslError::WantRead) => {
          self.front_interest.insert(EventSet::readable());
          println!("writable WantRead");
        },
        Err(NonblockingSslError::WantWrite) => {
          self.front_interest.insert(EventSet::writable());
          println!("writable WantWrite");
        },
        Err(e) => {
          panic!("not implemented; client err={:?}", e);
        }
      }
      self.front_buf = Some(buf);
    } else {
      println!("FRONT [{:?}]: front_mut_buf unavailable", self.token);
    }
    if let Some(frontend_token) = self.token {
      event_loop.reregister(self.stream.get_ref(), frontend_token, self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    }
    Ok(())
  }

  // Forward content to application
  fn back_writable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    if let Some(mut buf) = self.front_buf.take() {
      //println!("in back_writable 2: front_buf contains {} bytes", buf.remaining());

      let mut b = buf.flip();
      if let Some(ref mut sock) = self.backend {
        match sock.try_write_buf(&mut b) {
          Ok(None) => {
            println!("client flushing buf; WOULDBLOCK");

            self.back_interest.insert(EventSet::writable());
          }
          Ok(Some(r)) => {
            //FIXME what happens if not everything was written?
            //if let Some((front,back)) = self.tokens() {
            //  println!("BACK [{}->{}]: read {} bytes", front.as_usize(), back.as_usize(), r);
            //}

            self.front_interest.insert(EventSet::readable());
            self.back_interest.remove(EventSet::writable());
            self.back_interest.insert(EventSet::readable());
          }
          Err(e) =>  println!("not implemented; client err={:?}", e),
        }
      }
      self.front_buf = Some(b.flip());
    } else {
      println!("BACK [{:?}]: front_buf unavailable", self.token);
    }

    if let Some((frontend_token,backend_token)) = self.tokens() {
      if let Some(ref sock) = self.backend {
        event_loop.reregister(sock, backend_token, self.back_interest, PollOpt::edge()).unwrap();
      }
      event_loop.reregister(self.stream.get_ref(), frontend_token, self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    }
    Ok(())
  }

  // Read content from application
  fn back_readable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    if let Some(mut buf) = self.back_buf.take() {
      //println!("in back_readable(): back_mut_buf contains {} bytes", buf.remaining());

      if let Some(ref mut sock) = self.backend {
        match sock.try_read_buf(&mut buf) {
          Ok(None) => {
            println!("We just got readable, but were unable to read from the socket?");
          }
          Ok(Some(r)) => {
            //if let Some((front,back)) = self.tokens() {
            //  println!("BACK [{}<-{}]: read {} bytes", front.as_usize(), back.as_usize(), r);
            //}
            self.back_interest.remove(EventSet::readable());
            self.front_interest.insert(EventSet::writable());
          }
          Err(e) => {
            println!("not implemented; client err={:?}", e);
            //self.interest.remove(EventSet::readable());
          }
        };
      }
      self.back_buf = Some(buf);
    }

    if let Some((frontend_token,backend_token)) = self.tokens() {
      if let Some(ref sock) = self.backend {
        event_loop.reregister(sock, backend_token, self.back_interest, PollOpt::edge()).unwrap();
      }
      event_loop.reregister(self.stream.get_ref(), frontend_token, self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    }
    Ok(())
  }
}


pub struct ApplicationListener {
  sock:           TcpListener,
  token:          Token,
  front_address:  SocketAddr
}

type ClientToken = Token;

pub struct Server {
  instances:       HashMap<String, Vec<SocketAddr>>,
  listener:        ApplicationListener,
  fronts:          HashMap<String, Vec<HttpFront>>,
  clients:         Slab<Client>,
  backend:         Slab<ClientToken>,
  context:         SslContext,
  max_listeners:   usize,
  max_connections: usize,
  tx:              mpsc::Sender<ServerMessage>
}

const s: &'static str = "pouet";

impl Server {
  fn new(listener: ApplicationListener, max_connections: usize, tx: mpsc::Sender<ServerMessage>) -> Server {
    let mut context = SslContext::new(SslMethod::Tlsv1).unwrap();
    //let mut context = SslContext::new(SslMethod::Sslv3).unwrap();
    context.set_certificate_file("assets/certificate.pem", X509FileType::PEM);
    context.set_private_key_file("assets/key.pem", X509FileType::PEM);

    fn servername_callback(ssl: &mut Ssl, ad: &mut i32) -> i32 {
      println!("GOT SERVER NAME: {:?}", ssl.get_servername());
      0
    }
    context.set_servername_callback(Some(servername_callback as ServerNameCallback));

    /*
    fn servername_callback_s(ssl: &mut Ssl, ad: &mut i32, data: &&str) -> i32 {
      println!("got data: {}", *data);
      println!("GOT SERVER NAME: {:?}", ssl.get_servername());
      0
    }
    context.set_servername_callback_with_data(servername_callback_s as ServerNameCallbackData<&str>, s);
    */

    Server {
      instances:       HashMap::new(),
      listener:        listener,
      fronts:          HashMap::new(),
      clients:         Slab::new_starting_at(Token(1), max_connections),
      backend:         Slab::new_starting_at(Token(1 + max_connections), max_connections),
      context:         context,
      max_listeners:   1,
      max_connections: max_connections,
      tx:              tx
    }
  }

  pub fn close_client(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {
    self.clients[token].stream.get_ref().shutdown(Shutdown::Both);
    event_loop.deregister(self.clients[token].stream.get_ref());
    if let Some(ref sock) = self.clients[token].backend {
      sock.shutdown(Shutdown::Both);
      event_loop.deregister(sock);
    }

    if let Some(backend_token) = self.clients[token].backend_token {
      if self.backend.contains(backend_token) {
        self.backend.remove(backend_token);
      }
    }
    self.clients.remove(token);
  }

  pub fn add_http_front(&mut self, http_front: HttpFront, event_loop: &mut EventLoop<Server>) {
    let front2 = http_front.clone();
    let front3 = http_front.clone();
    if let Some(fronts) = self.fronts.get_mut(&http_front.hostname) {
        fronts.push(front2);
    }

    if self.fronts.get(&http_front.hostname).is_none() {
      self.fronts.insert(http_front.hostname, vec![front3]);
    }
  }

  pub fn remove_http_front(&mut self, front: HttpFront, event_loop: &mut EventLoop<Server>) {
    println!("removing http_front {:?}", front);
    if let Some(fronts) = self.fronts.get_mut(&front.hostname) {
      fronts.retain(|f| f != &front);
    }
  }

  pub fn add_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut EventLoop<Server>) {
    if let Some(addrs) = self.instances.get_mut(app_id) {
        addrs.push(*instance_address);
    }

    if self.instances.get(app_id).is_none() {
      self.instances.insert(String::from(app_id), vec![*instance_address]);
    }
  }

  pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut EventLoop<Server>) {
      if let Some(instances) = self.instances.get_mut(app_id) {
        instances.retain(|addr| addr != instance_address);
      } else {
        println!("Instance was already removed");
      }
  }

  pub fn backend_from_request(&self, host: &str, uri: &str) -> Option<SocketAddr> {
    println!("Getting a backend for {}", host);
    if let Some(http_fronts) = self.fronts.get(host) {
      // ToDo get the front with the most specific matching path_begin
      println!("Choosing a front from {:?}", http_fronts);
      if let Some(http_front) = http_fronts.get(0) {
        // ToDo round-robin on instances
        println!("Choosing an instance from {:?}", self.instances.get(&http_front.app_id));
        if let Some(app_instances) = self.instances.get(&http_front.app_id) {
          let rnd = random::<usize>();
          let idx = rnd % app_instances.len();
          app_instances.get(idx).map(|& addr| addr)
        } else {
          None
        }
      } else {
        None
      }
    } else {
      None
    }
  }

  pub fn accept(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {
    let application_listener = &self.listener;
    let accepted = application_listener.sock.accept();

    if let Ok(Some((frontend_sock, _))) = accepted {
      if let Ok(ssl) = Ssl::new(&self.context) {
        //if let Ok(ssl_sock) = frontend_sock.try_clone() {
          if let Ok(stream) = NonblockingSslStream::accept(ssl, frontend_sock) {
            if let Some(client) = Client::new(stream) {
              if let Ok(client_token) = self.clients.insert(client) {
                event_loop.register(self.clients[client_token].stream.get_ref(), client_token, EventSet::readable(), PollOpt::edge()).unwrap();
                self.clients[client_token].set_front_token(client_token);
                self.clients[client_token].status = ConnectionStatus::ClientConnected;
              } else {
                println!("could not add client to slab");
              }
            } else {
              println!("could not create a client");
            }
          } else {
            println!("could not create ssl stream");
          }
        //} else {
        //  println!("could not clone socket");
        //}
      } else {
        println!("could not create ssl context");
      }
    } else {
      println!("could not accept connection: {:?}", accepted);
    }
  }

  pub fn connect_to_backend(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {
    if let (Some(host), Some(uri)) = (self.clients[token].http_state.get_host(), self.clients[token].http_state.get_uri()) {
      if let Some(back) = self.backend_from_request(&host, &uri) {
        if let Ok(socket) = TcpStream::connect(&back) {
          if let Ok(backend_token) = self.backend.insert(token) {
            //println!("backend connected and stored");
            self.clients[token].backend       = Some(socket);
            self.clients[token].backend_token = Some(backend_token);
            self.clients[token].status        = ConnectionStatus::Connected;

            if let Some(ref sock) = self.clients[token].backend {
              event_loop.register(sock, backend_token, EventSet::writable(), PollOpt::edge()).unwrap();
            }
            //FIXME: maybe not the right place to change state
            if let Some(rl) = self.clients[token].http_state.get_request_line() {
              self.clients[token].http_state = HttpState::Proxying(rl, host);
            }
            return;
          }
        }
      }
    }
    self.close_client(event_loop, token);
  }
}

impl Handler for Server {
  type Timeout = usize;
  type Message = HttpProxyOrder;

  fn ready(&mut self, event_loop: &mut EventLoop<Server>, token: Token, events: EventSet) {
    //println!("{:?} got events: {:?}", token, events);
    if events.is_readable() {
      println!("REA({})", token.as_usize());
      //println!("{:?} is readable", token);
      if token == Token(0) {
        self.accept(event_loop, token)
      } else if token.as_usize() < self.max_listeners + self.max_connections {
        if self.clients.contains(token) {
          self.clients[token].readable(event_loop);

          if let HttpState::HasHost(_,_,_) = self.clients[token].http_state {
            self.connect_to_backend(event_loop, token);
          } else if let HttpState::Error(_) = self.clients[token].http_state {
            self.close_client(event_loop, token);
          }
        } else {
          println!("client {:?} was removed", token);
        }
      } else if token.as_usize() < self.max_listeners + 2 * self.max_connections {
        if self.backend.contains(token) {
          let tok = self.backend[token];
          if self.clients.contains(tok) {
            self.clients[tok].back_readable(event_loop);
          } else {
            println!("client {:?} was removed", token);
          }
        } else {
          println!("backend {:?} was removed", token);
        }
      }
    }

    if events.is_writable() {
      //println!("{:?} is writable", token);
      println!("WRI({})", token.as_usize());
      if token.as_usize() < self.max_listeners {
        println!("received writable for listener {:?}, this should not happen", token);
      } else  if token.as_usize() < self.max_listeners + self.max_connections {
        if self.clients.contains(token) {
          self.clients[token].writable(event_loop);
        } else {
          println!("client {:?} was removed", token);
        }
      } else if token.as_usize() < self.max_listeners + 2 * self.max_connections {
        if self.backend.contains(token) {
          let tok = self.backend[token];
          if self.clients.contains(tok) {
            self.clients[tok].back_writable(event_loop);
          } else {
            println!("client {:?} was removed", token);
          }
        } else {
          println!("backend {:?} was removed", token);
        }
      }
    }

    if events.is_hup() {
      println!("HUP({})", token.as_usize());
      if token == Token(0) {
        println!("should not happen: server {:?} closed", token);
      } else if token.as_usize() < self.max_listeners + self.max_connections {
        if self.clients.contains(token) {
          println!("client {:?} got hup", token.as_usize());
          if  self.clients[token].status == ConnectionStatus::ServerClosed ||
              self.clients[token].status == ConnectionStatus::ClientConnected { // the server never answered, the client closed
            self.clients[token].status = ConnectionStatus::Closed;
            self.close_client(event_loop, token);
            println!("removed");
          } else {
            self.clients[token].status = ConnectionStatus::ClientClosed;
          }
          //self.clients[token].close();
        } else {
          println!("client {:?} was already removed", token);
        }
      } else if token.as_usize() < self.max_listeners + 2 * self.max_connections {
        if self.backend.contains(token) {
          let tok = self.backend[token];
          if self.clients.contains(tok) {
            println!("server {} got hup (for client {})", token.as_usize(), tok.as_usize());
            println!("removing server {:?}", token);
            if self.clients[tok].status == ConnectionStatus::ClientClosed {
              self.clients[tok].status = ConnectionStatus::Closed;
              self.close_client(event_loop, tok);
              println!("removed");
            } else {
              self.clients[tok].status = ConnectionStatus::ServerClosed;
            }
            //self.clients[tok].close();
          } else {
            println!("client {:?} was already removed", token);
          }
        } else {

          println!("backend {:?} was already removed", token);
        }
      }
      println!("end_hup");
    }
  }

  fn notify(&mut self, event_loop: &mut EventLoop<Self>, message: Self::Message) {
  // ToDo temporary
    println!("notified: {:?}", message);
    match message {
      HttpProxyOrder::Command(Command::AddHttpFront(front)) => {
        println!("add front {:?}", front);
          self.add_http_front(front, event_loop);
          self.tx.send(ServerMessage::AddedHttpFront);
      },
      HttpProxyOrder::Command(Command::RemoveHttpFront(front)) => {
        println!("remove front {:?}", front);
        self.remove_http_front(front, event_loop);
        self.tx.send(ServerMessage::RemovedHttpFront);
      },
      HttpProxyOrder::Command(Command::AddInstance(instance)) => {
        println!("add instance {:?}", instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.add_instance(&instance.app_id, &addr, event_loop);
          self.tx.send(ServerMessage::AddedInstance);
        }
      },
      HttpProxyOrder::Command(Command::RemoveInstance(instance)) => {
        println!("remove instance {:?}", instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.remove_instance(&instance.app_id, &addr, event_loop);
          self.tx.send(ServerMessage::RemovedInstance);
        }
      },
      HttpProxyOrder::Stop                   => {
        event_loop.shutdown();
      },
      _ => {
        println!("unsupported message, ignoring");
      }
    }
  }

  fn timeout(&mut self, event_loop: &mut EventLoop<Self>, timeout: Self::Timeout) {
    println!("timeout");
  }

  fn interrupted(&mut self, event_loop: &mut EventLoop<Self>) {
    println!("interrupted");
  }
}

pub fn start() {
  // ToDo temporary
  let mut event_loop = EventLoop::new().unwrap();

  let (tx,rx) = channel::<ServerMessage>();
  let channel = event_loop.channel();
  let notify_tx = tx.clone();
  let front: SocketAddr = FromStr::from_str("127.0.0.1:8080").unwrap();

  let tcp_listener = TcpListener::bind(&front).unwrap();
  let listener = ApplicationListener {
    sock:           tcp_listener,
    token:          Token(0),
    front_address:  front
  };

  event_loop.register(&listener.sock, listener.token, EventSet::readable(), PollOpt::edge()).unwrap();

  let mut server = Server::new(listener, 500, tx);

  let join_guard = thread::spawn(move|| {
    println!("starting event loop");
    event_loop.run(&mut server).unwrap();
    println!("ending event loop");
    notify_tx.send(ServerMessage::Stopped);
  });


  //println!("listen for connections");
  //event_loop.register(&listener, SERVER, EventSet::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
  //let mut s = Server::new(10, 500, tx);
  //{
  //  let back: SocketAddr = FromStr::from_str("127.0.0.1:5678").unwrap();
  //  s.add_tcp_front(1234, "yolo", &mut event_loop);
  //  s.add_instance("yolo", &back, &mut event_loop);
  //}
  //{
  //  let back: SocketAddr = FromStr::from_str("127.0.0.1:5678").unwrap();
  //  s.add_tcp_front(1235, "yolo", &mut event_loop);
  //  s.add_instance("yolo", &back, &mut event_loop);
  //}
  //thread::spawn(move|| {
  //  println!("starting event loop");
  //  event_loop.run(&mut s).unwrap();
  //  println!("ending event loop");
  //});
}

pub fn start_listener(front: SocketAddr, max_listeners: usize, max_connections: usize, tx: mpsc::Sender<ServerMessage>) -> (Sender<HttpProxyOrder>,thread::JoinHandle<()>)  {
  let mut event_loop = EventLoop::new().unwrap();
  let channel = event_loop.channel();
  let notify_tx = tx.clone();

  let tcp_listener = TcpListener::bind(&front).unwrap();
  let listener = ApplicationListener {
    sock:           tcp_listener,
    token:          Token(0),
    front_address:  front
  };

  event_loop.register(&listener.sock, listener.token, EventSet::readable(), PollOpt::edge()).unwrap();

  let mut server = Server::new(listener, max_connections, tx);

  let join_guard = thread::spawn(move|| {
    println!("starting event loop");
    event_loop.run(&mut server).unwrap();
    println!("ending event loop");
    notify_tx.send(ServerMessage::Stopped);
  });

  (channel, join_guard)
}

#[cfg(test)]
mod tests {
  extern crate tiny_http;
  use super::*;
  use std::net::{TcpListener, TcpStream, Shutdown};
  use std::io::{Read,Write};
  use std::{thread,str};
  use std::sync::mpsc::channel;
  use std::net::SocketAddr;
  use std::str::FromStr;
  use std::time::Duration;
  use messages::{Command,HttpFront,Instance};

  /*
  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn mi() {
    thread::spawn(|| { start_server(); });
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1024").unwrap();
    let (tx,rx) = channel::<ServerMessage>();
    let (sender, jg) = start_listener(front, 10, 10, tx.clone());
    let front = HttpFront { app_id: String::from("app_1"), hostname: String::from("localhost:1024"), path_begin: String::from("/") };
    sender.send(HttpProxyOrder::Command(Command::AddHttpFront(front)));
    let instance = Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1025 };
    sender.send(HttpProxyOrder::Command(Command::AddInstance(instance)));
    println!("test received: {:?}", rx.recv());
    println!("test received: {:?}", rx.recv());
    thread::sleep_ms(300);

    let mut client = TcpStream::connect(("127.0.0.1", 1024)).unwrap();
    // 5 seconds of timeout
    client.set_read_timeout(Some(Duration::new(5,0)));
    thread::sleep_ms(100);
    let mut w  = client.write(&b"GET / HTTP/1.1\r\nHost: localhost:1024\r\nConnection: Close\r\n\r\n"[..]);
    println!("http client write: {:?}", w);
    let mut buffer = [0;4096];
    thread::sleep_ms(500);
    let mut r = client.read(&mut buffer[..]);
    println!("http client read: {:?}", r);
    match r {
      Err(e)      => assert!(false, "client request should not fail. Error: {:?}",e),
      Ok(sz) => {
        // Read the Response.
        println!("read response");

        println!("Response: {}", str::from_utf8(&buffer[..]).unwrap());

        //thread::sleep_ms(300);
        //assert_eq!(&body, &"Hello World!"[..]);
        assert_eq!(sz, 154);
        //assert!(false);
      }
    }
  }

  use self::tiny_http::{ServerBuilder, Response};

  #[allow(unused_mut, unused_must_use, unused_variables)]
  fn start_server() {
    thread::spawn(move|| {
      let server = ServerBuilder::new().with_port(1025).build().unwrap();
      println!("starting web server");

      for request in server.incoming_requests() {
        println!("backend web server got request -> method: {:?}, url: {:?}, headers: {:?}",
          request.method(),
          request.url(),
          request.headers()
        );

        let response = Response::from_string("hello world");
        request.respond(response);
        println!("backend web server sent response");
      }
    });
  }
*/
}