use super::{Fault, Worker};
use crate::experimental_lsm::{
    mempool::Mempool,
    query::{merkle, Snapshot},
};
use anyhow::{ensure, Context, Result};
use bitcoin::{
    consensus::{deserialize, serialize},
    hashes::{sha256, Hash},
    BlockHash, Transaction, Txid,
};
use serde_json::{json, Value};

fn invalid() -> anyhow::Error {
    Fault(-32602, "invalid parameters").into()
}
fn argument<'a>(params: &'a Value, index: usize, name: &str) -> Option<&'a Value> {
    if params.is_array() {
        params.get(index)
    } else {
        params.get(name)
    }
}
fn text<'a>(params: &'a Value, index: usize, name: &str) -> Result<&'a str> {
    argument(params, index, name)
        .and_then(Value::as_str)
        .ok_or_else(invalid)
}
fn number(params: &Value, index: usize, name: &str) -> Result<u32> {
    argument(params, index, name)
        .and_then(Value::as_u64)
        .and_then(|value| value.try_into().ok())
        .ok_or_else(invalid)
}
fn optional_number(params: &Value, index: usize, name: &str) -> Result<u32> {
    if argument(params, index, name).is_some() {
        number(params, index, name)
    } else {
        Ok(0)
    }
}
fn boolean(params: &Value, index: usize, name: &str) -> Result<bool> {
    argument(params, index, name).map_or(Ok(false), |value| value.as_bool().ok_or_else(invalid))
}
fn txid(params: &Value) -> Result<Txid> {
    text(params, 0, "tx_hash")?.parse().map_err(|_| invalid())
}
pub(super) fn wire_script(params: &Value) -> Result<(String, [u8; 32])> {
    let wire = text(params, 0, "scripthash")?;
    ensure!(wire.len() == 64, invalid());
    let mut raw: [u8; 32] = hex::decode(wire)
        .map_err(|_| invalid())?
        .try_into()
        .map_err(|_| invalid())?;
    raw.reverse();
    Ok((wire.to_ascii_lowercase(), raw))
}
pub(super) fn validate(method: &str, params: &Value) -> Result<()> {
    let (minimum, names): (usize, &[&str]) = match method {
        "server.version" => (0, &["client_name", "protocol_version"]),
        "server.ping"
        | "server.features"
        | "server.health"
        | "server.banner"
        | "server.donation_address"
        | "server.peers.subscribe"
        | "blockchain.headers.subscribe"
        | "blockchain.relayfee"
        | "mempool.get_fee_histogram" => (0, &[]),
        "server.add_peer" => (1, &["features"]),
        "blockchain.scripthash.get_balance"
        | "blockchain.scripthash.get_history"
        | "blockchain.scripthash.get_mempool"
        | "blockchain.scripthash.listunspent"
        | "blockchain.scripthash.subscribe"
        | "blockchain.scripthash.unsubscribe" => (1, &["scripthash"]),
        "blockchain.block.header" => (1, &["height", "cp_height"]),
        "blockchain.block.headers" => (2, &["start_height", "count", "cp_height"]),
        "blockchain.transaction.get" => (1, &["tx_hash", "verbose"]),
        "blockchain.transaction.get_merkle" => (2, &["tx_hash", "height"]),
        "blockchain.transaction.id_from_pos" => (2, &["height", "tx_pos", "merkle"]),
        "blockchain.transaction.broadcast" => (1, &["raw_tx"]),
        "blockchain.estimatefee" => (1, &["number"]),
        _ => return Err(Fault(-32601, "method not found in Electrum 1.4").into()),
    };
    match params {
        Value::Null => ensure!(minimum == 0, invalid()),
        Value::Array(values) => ensure!(
            values.len() >= minimum && values.len() <= names.len(),
            invalid()
        ),
        Value::Object(values) => {
            ensure!(
                values.keys().all(|key| names.contains(&key.as_str())),
                invalid()
            );
            ensure!(
                names
                    .iter()
                    .take(minimum)
                    .all(|name| values.contains_key(*name)),
                invalid()
            );
        }
        _ => return Err(invalid()),
    }
    match method {
        method if method.starts_with("blockchain.scripthash.") => {
            wire_script(params)?;
        }
        "blockchain.block.header" => {
            number(params, 0, "height")?;
            optional_number(params, 1, "cp_height")?;
        }
        "blockchain.block.headers" => {
            number(params, 0, "start_height")?;
            number(params, 1, "count")?;
            optional_number(params, 2, "cp_height")?;
        }
        "blockchain.transaction.get" => {
            txid(params)?;
            boolean(params, 1, "verbose")?;
        }
        "blockchain.transaction.get_merkle" => {
            txid(params)?;
            number(params, 1, "height")?;
        }
        "blockchain.transaction.id_from_pos" => {
            number(params, 0, "height")?;
            number(params, 1, "tx_pos")?;
            boolean(params, 2, "merkle")?;
        }
        "blockchain.transaction.broadcast" => {
            ensure!(
                text(params, 0, "raw_tx")?.len() <= crate::MAX_BLOCK_BYTES * 2,
                invalid()
            );
        }
        "blockchain.estimatefee" => {
            ensure!(number(params, 0, "number")? > 0, invalid());
        }
        _ => {}
    }
    Ok(())
}
fn version_tuple(value: &str) -> Result<[u32; 3]> {
    ensure!(value.len() <= 16, invalid());
    let fields: Vec<_> = value.split('.').collect();
    ensure!((2..=3).contains(&fields.len()), invalid());
    let mut result = [0; 3];
    for (slot, field) in result.iter_mut().zip(fields) {
        *slot = field.parse().map_err(|_| invalid())?;
    }
    Ok(result)
}
pub(super) fn version(params: &Value) -> Result<Value> {
    if let Some(client) = argument(params, 0, "client_name") {
        ensure!(
            client.as_str().is_some_and(|value| value.len() <= 256),
            invalid()
        );
    }
    let request = argument(params, 1, "protocol_version")
        .cloned()
        .unwrap_or(json!("1.4"));
    let (minimum, maximum) = if let Some(version) = request.as_str() {
        (version_tuple(version)?, version_tuple(version)?)
    } else {
        let values = request
            .as_array()
            .filter(|values| values.len() == 2)
            .ok_or_else(invalid)?;
        (
            version_tuple(values[0].as_str().ok_or_else(invalid)?)?,
            version_tuple(values[1].as_str().ok_or_else(invalid)?)?,
        )
    };
    ensure!(
        minimum <= [1, 4, 0] && maximum >= [1, 4, 0],
        Fault(1, "only Electrum protocol 1.4 is supported")
    );
    Ok(json!([
        "electrum-lite 0.0.1-beta.1 LSM experimental",
        "1.4"
    ]))
}
fn header(snapshot: &Snapshot<'_>) -> Result<Value> {
    let tip = snapshot
        .state()
        .tip
        .as_ref()
        .context("empty confirmed state")?;
    Ok(json!({"height":tip.height,"hex":hex::encode(serialize(&snapshot.header(tip.height)?))}))
}
fn display_hash(hash: [u8; 32]) -> String {
    BlockHash::from_byte_array(hash).to_string()
}
fn checkpoint(snapshot: &Snapshot<'_>, height: u32, cp: u32) -> Result<Value> {
    ensure!(height <= cp, invalid());
    let (branch, root) = merkle(snapshot.header_hashes(cp)?, height as usize)?;
    Ok(
        json!({"branch":branch.into_iter().map(display_hash).collect::<Vec<_>>(),"root":display_hash(root)}),
    )
}
fn status(
    pool: &Mempool,
    snapshot: &Snapshot<'_>,
    script: [u8; 32],
    limit: usize,
) -> Result<Value> {
    let (confirmed, pending) = pool.history(snapshot, script, limit)?;
    if confirmed.is_empty() && pending.is_empty() {
        return Ok(Value::Null);
    }
    let mut bytes = String::new();
    for entry in confirmed {
        use std::fmt::Write;
        write!(bytes, "{}:{}:", entry.txid, entry.height)?;
    }
    for entry in pending {
        use std::fmt::Write;
        write!(bytes, "{}:{}:", entry.txid, entry.height)?;
    }
    Ok(json!(sha256::Hash::hash(bytes.as_bytes()).to_string()))
}
fn verbose(
    tx: &Transaction,
    raw: &[u8],
    confirmed: Option<(u32, BlockHash, u32)>,
    snapshot: &Snapshot<'_>,
    chain: crate::Chain,
) -> Result<Value> {
    let vin:Vec<_>=tx.input.iter().map(|input| {
        let mut value=if tx.is_coinbase(){json!({"coinbase":hex::encode(input.script_sig.as_bytes()),"sequence":input.sequence.0})}else{json!({"txid":input.previous_output.txid,"vout":input.previous_output.vout,"sequence":input.sequence.0,"scriptSig":{"hex":hex::encode(input.script_sig.as_bytes()),"asm":input.script_sig.to_asm_string()}})};
        if !input.witness.is_empty(){value["txinwitness"]=json!(input.witness.iter().map(hex::encode).collect::<Vec<_>>());}value
    }).collect();
    let vout:Vec<_>=tx.output.iter().enumerate().map(|(index,output)| {
        let script=&output.script_pubkey;let kind=if script.is_p2pkh(){"pubkeyhash"}else if script.is_p2sh(){"scripthash"}else if script.is_p2wpkh(){"witness_v0_keyhash"}else if script.is_p2wsh(){"witness_v0_scripthash"}else if script.is_p2tr(){"witness_v1_taproot"}else if script.is_op_return(){"nulldata"}else{"nonstandard"};
        let mut description=json!({"hex":hex::encode(script.as_bytes()),"asm":script.to_asm_string(),"type":kind});
        if let Ok(address)=bitcoin::Address::from_script(script,chain.network()){description["address"]=json!(address.to_string());}
        json!({"n":index,"value":output.value.to_btc(),"scriptPubKey":description})
    }).collect();
    let mut result = json!({"txid":tx.compute_txid(),"hash":tx.compute_wtxid(),"hex":hex::encode(raw),"size":raw.len(),"vsize":tx.vsize(),"weight":tx.weight().to_wu(),"version":tx.version.0,"locktime":tx.lock_time.to_consensus_u32(),"vin":vin,"vout":vout});
    if let Some((height, hash, time)) = confirmed {
        let tip = snapshot.state().tip.as_ref().context("missing tip")?;
        result["blockhash"] = json!(hash);
        result["confirmations"] = json!(tip.height - height + 1);
        result["time"] = json!(time);
        result["blocktime"] = json!(time);
    } else {
        result["confirmations"] = json!(0);
    }
    Ok(result)
}
impl Worker {
    pub(super) fn dispatch(&mut self, method: &str, params: &Value) -> Result<Value> {
        let snapshot = self.store.snapshot()?;
        ensure!(
            self.pool.ready_for(&snapshot),
            Fault(
                -32000,
                "complete mempool snapshot for current generation required"
            )
        );
        match method {
            "__subscribe"=>{let value=status(&self.pool,&snapshot,wire_script(params)?.1,self.options.query_limit)?;Ok(json!({"status":value,"tip":snapshot.state().tip.as_ref().context("empty index")?.hash}))},
            "__poll"=>{
                let scripts=params.as_array().context("invalid internal poll")?;ensure!(scripts.len()<=self.options.subscriptions,"poll subscription budget exceeded");
                let mut statuses=serde_json::Map::new();for wire in scripts {let (wire,raw)=wire_script(&json!([wire]))?;statuses.insert(wire,status(&self.pool,&snapshot,raw,self.options.query_limit)?);}
                Ok(json!({"tip":snapshot.state().tip.as_ref().context("empty index")?.hash,"header":header(&snapshot)?,"statuses":statuses}))
            },
            "blockchain.headers.subscribe"=>header(&snapshot),
            "blockchain.scripthash.subscribe"=>status(&self.pool,&snapshot,wire_script(params)?.1,self.options.query_limit),
            "blockchain.scripthash.get_balance"=>{let (confirmed,unconfirmed)=self.pool.balance(&snapshot,wire_script(params)?.1)?;Ok(json!({"confirmed":confirmed,"unconfirmed":unconfirmed}))},
            "blockchain.scripthash.get_history"=>{let (confirmed,pending)=self.pool.history(&snapshot,wire_script(params)?.1,self.options.query_limit)?;Ok(json!(confirmed.into_iter().map(|entry|json!({"tx_hash":entry.txid,"height":entry.height})).chain(pending.into_iter().map(|entry|json!({"tx_hash":entry.txid,"height":entry.height,"fee":entry.fee}))).collect::<Vec<_>>()))},
            "blockchain.scripthash.get_mempool"=>{let (_,pending)=self.pool.history(&snapshot,wire_script(params)?.1,self.options.query_limit)?;Ok(json!(pending.into_iter().map(|entry|json!({"tx_hash":entry.txid,"height":entry.height,"fee":entry.fee})).collect::<Vec<_>>()))},
            "blockchain.scripthash.listunspent"=>Ok(json!(self.pool.list_unspent(&snapshot,wire_script(params)?.1,self.options.query_limit)?.into_iter().map(|(outpoint,coin)|json!({"tx_hash":outpoint.txid,"tx_pos":outpoint.vout,"height":coin.height,"value":coin.value})).collect::<Vec<_>>())),
            "blockchain.block.header"=>{let height=number(params,0,"height")?;let cp=optional_number(params,1,"cp_height")?;let bytes=hex::encode(serialize(&snapshot.header(height)?));if cp==0 {Ok(json!(bytes))}else{let mut result=checkpoint(&snapshot,height,cp)?;result["header"]=json!(bytes);Ok(result)}},
            "blockchain.block.headers"=>{
                let start=number(params,0,"start_height")?;let requested=number(params,1,"count")?.min(2016);let cp=optional_number(params,2,"cp_height")?;
                let height=snapshot.state().tip.as_ref().context("empty index")?.height;let count=(u64::from(height)+1).saturating_sub(u64::from(start)).min(u64::from(requested)) as u32;
                let mut result=json!({"count":count,"hex":hex::encode(snapshot.headers(start,count)?),"max":2016});
                if count>0&&cp>0 {let proof=checkpoint(&snapshot,start.checked_add(count-1).context("header range overflow")?,cp)?;result["branch"]=proof["branch"].clone();result["root"]=proof["root"].clone();}Ok(result)
            },
            "blockchain.transaction.get"=>{
                let id=txid(params)?;let (raw,confirmation)=if let Ok(raw)=self.pool.transaction(&snapshot,id,crate::MAX_BLOCK_BYTES){(raw,None)}else{let (occurrence,raw)=snapshot.transaction(id,None,crate::MAX_BLOCK_BYTES)?;let time=snapshot.header(occurrence.height)?.time;(raw,Some((occurrence.height,occurrence.block_hash,time)))};
                if boolean(params,1,"verbose")? {verbose(&deserialize(&raw)?,&raw,confirmation,&snapshot,self.options.chain)}else{Ok(json!(hex::encode(raw)))}
            },
            "blockchain.transaction.get_merkle"=>{let proof=snapshot.proof(txid(params)?,number(params,1,"height")?)?;Ok(json!({"block_height":proof.occurrence.height,"pos":proof.occurrence.position,"merkle":proof.branch.into_iter().map(display_hash).collect::<Vec<_>>()}))},
            "blockchain.transaction.id_from_pos"=>{let height=number(params,0,"height")?;let position=number(params,1,"tx_pos")?;let block=snapshot.block(height)?;let txid=block.txdata.get(position as usize).context("transaction position out of range")?.compute_txid();if boolean(params,2,"merkle")? {let proof=snapshot.proof(txid,height)?;Ok(json!({"tx_hash":txid,"merkle":proof.branch.into_iter().map(display_hash).collect::<Vec<_>>()}))}else{Ok(json!(txid))}},
            "blockchain.transaction.broadcast"=>{let raw=text(params,0,"raw_tx")?;ensure!(raw.len()<=crate::MAX_BLOCK_BYTES*2,invalid());let bytes=hex::decode(raw).map_err(|_|invalid())?;let tx:Transaction=deserialize(&bytes).map_err(|_|invalid())?;ensure!(serialize(&tx)==bytes,invalid());let id:String=self.options.core.call("sendrawtransaction",json!([raw])).map_err(|_|Fault(1,"Core rejected transaction or RPC is unavailable"))?;ensure!(id==tx.compute_txid().to_string(),"Core broadcast TXID mismatch");self.pool.invalidate();if let Ok(mut health)=self.health.write(){health.data["ready"]=json!(false);}Ok(json!(id))},
            "mempool.get_fee_histogram"=>Ok(json!(self.pool.fee_histogram(&snapshot)?)),
            "blockchain.relayfee"=>{let info:Value=self.options.core.call("getnetworkinfo",json!([]))?;Ok(json!(info["relayfee"].as_f64().context("relay fee missing")?))},
            "blockchain.estimatefee"=>{let target=number(params,0,"number")?;ensure!(target>0,invalid());let estimate:Value=self.options.core.call("estimatesmartfee",json!([target]))?;Ok(estimate.get("feerate").cloned().unwrap_or(json!(-1)))},
            _=>Err(Fault(-32601,"method not found in Electrum 1.4").into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn negotiation_and_named_params_only_accept_baseline_1_4() -> Result<()> {
        assert_eq!(version(&json!(["test", ["1.4", "1.6"]]))?[1], "1.4");
        assert!(version(&json!(["test", "1.6"])).is_err());
        assert!(validate(
            "blockchain.block.headers",
            &json!({"start_height":0,"count":1,"headers":true})
        )
        .is_err());
        assert!(validate(
            "blockchain.scripthash.get_balance",
            &json!({"scripthash":"00".repeat(32)})
        )
        .is_ok());
        assert!(validate("blockchain.scriptpubkey.get_balance", &json!([])).is_err());
        let (wire, raw) = wire_script(&json!([format!("{}01", "00".repeat(31))]))?;
        assert_eq!(wire.len(), 64);
        assert_eq!(raw[0], 1);
        Ok(())
    }
}
