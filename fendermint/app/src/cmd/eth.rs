// Copyright 2022-2023 Protocol Labs
// SPDX-License-Identifier: Apache-2.0, MIT

use fendermint_rpc::client::http_client;
use tendermint_rpc::HttpClient;

use crate::{
    cmd,
    options::eth::{EthArgs, EthCommands},
    settings::EthSettings,
};

cmd! {
  EthArgs(self, settings: EthSettings) {
    match self.command.clone() {
      EthCommands::Run { url, proxy_url } => {
        let client = http_client(url, proxy_url)?;
        run(client, settings).await
      }
    }
  }
}

/// Run the Ethereum
async fn run(_client: HttpClient, settings: EthSettings) -> anyhow::Result<()> {
    fendermint_eth_api::listen(settings.http.addr()).await
}