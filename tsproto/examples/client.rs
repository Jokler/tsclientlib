extern crate base64;
extern crate failure;
extern crate futures;
extern crate gstreamer as gst;
extern crate gstreamer_app as gst_app;
extern crate gstreamer_audio as gst_audio;
extern crate ring;
#[macro_use]
extern crate slog;
extern crate slog_async;
extern crate slog_perf;
extern crate slog_term;
extern crate structopt;
extern crate tokio;
extern crate tsproto;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use futures::{future, Future, Sink};
use slog::Drain;
use structopt::clap::AppSettings;
use structopt::StructOpt;
use tokio::timer::Delay;
use tsproto::packets::*;
use tsproto::*;

mod utils;
use utils::*;

#[derive(StructOpt, Debug)]
#[structopt(raw(
	global_settings = "&[AppSettings::ColoredHelp, \
	                   AppSettings::VersionlessSubcommands]"
))]
struct Args {
	#[structopt(
		short = "a",
		long = "address",
		default_value = "127.0.0.1:9987",
		help = "The address of the server to connect to"
	)]
	address: SocketAddr,
	#[structopt(
		long = "local-address",
		default_value = "0.0.0.0:0",
		help = "The listening address of the client"
	)]
	local_address: SocketAddr,
	#[structopt(
		short = "v",
		long = "verbose",
		help = "Print the content of all packets",
		parse(from_occurrences)
	)]
	verbose: u8,
	// 0. Print nothing
	// 1. Print command string
	// 2. Print packets
	// 3. Print udp packets
}

fn main() {
	tsproto::init().unwrap();

	// Parse command line options
	let args = Args::from_args();

	let logger = {
		let decorator = slog_term::TermDecorator::new().build();
		let drain = slog_term::CompactFormat::new(decorator).build().fuse();
		let drain = slog_async::Async::new(drain).build().fuse();

		slog::Logger::root(drain, o!())
	};

	tokio::run(
		future::lazy(move || {
			let c = create_client(
				args.local_address,
				logger.clone(),
				SimplePacketHandler,
				args.verbose,
			);

			// Connect
			let logger2 = logger.clone();
			connect(logger.clone(), c.clone(), args.address)
				.map_err(|e| panic!("Failed to connect ({:?})", e))
				.and_then(move |con| {
					info!(logger2, "Connected");
					// Wait some time
					Delay::new(Instant::now() + Duration::from_secs(2))
						.map(move |_| con)
				}).and_then(move |con| {
					info!(logger, "Waited");

					// Send packet
					let mut header = Header::default();
					header.set_type(PacketType::Command);
					let mut cmd = commands::Command::new("sendtextmessage");

					cmd.push("targetmode", "3");
					cmd.push("msg", "Hello");

					let packet = Packet::new(header, Data::Command(cmd));
					con.as_packet_sink()
						.send(packet)
						.map(|_| ())
						.map_err(|e| panic!("Failed to send packet ({:?})", e))
						.and_then(|_| {
							Delay::new(Instant::now() + Duration::from_secs(3))
						}).and_then(move |_| {
							// Disconnect
							disconnect(con).map_err(|e| {
								panic!("Failed to disconnect ({:?})", e)
							})
						}).and_then(move |_| {
							info!(logger, "Disconnected");
							// Quit client
							drop(c);
							Ok(())
						})
				})
		}).map_err(|e| panic!("An error occurred {:?}", e)),
	);
}
