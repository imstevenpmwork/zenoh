//
// Copyright (c) 2017, 2020 ADLINK Technology Inc.
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ADLINK zenoh team, <zenoh@adlink-labs.tech>
//
use clap::App;
use async_std::prelude::*;
use async_std::task;
use zenoh::net::*;

fn main() {
    task::block_on( async {
        // initiate logging
        env_logger::init();

        let args = App::new("zenoh-net sub example")
            .arg("-l, --locator=[LOCATOR]   'Sets the locator used to initiate the zenoh session'")
            .arg("-s, --selector=[SELECTOR] 'Sets the selection of resources to subscribe'")
            .get_matches();

        let locator  = args.value_of("locator").unwrap_or("").to_string();
        let selector = args.value_of("selector").unwrap_or("/demo/example/**").to_string();

        println!("Openning session...");
        let session = open(&locator, None).await.unwrap();

        println!("Declaring Subscriber on {}", selector);

        let sub_info = SubInfo {
            reliability: Reliability::Reliable,
            mode: SubMode::Push,
            period: None
        };

        let data_handler = move |res_name: &str, payload: RBuf, data_info: Option<RBuf>| {
            println!(">> [Subscription listener] Received ('{}': '{}')", res_name, String::from_utf8_lossy(&payload.to_vec()));
            if let Some(mut info) = data_info {
                let _info = info.read_datainfo();
            }
        };

        let sub = session.declare_subscriber(&selector.into(), &sub_info, data_handler).await.unwrap();

        let mut stdin = async_std::io::stdin();
        let mut input = [0u8];
        while input[0] != 'q' as u8 {
            stdin.read_exact(&mut input).await.unwrap();
        }

        session.undeclare_subscriber(sub).await.unwrap();
        session.close().await.unwrap();
    })
}
