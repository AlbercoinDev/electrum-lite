"""Protocol integration, called by core_smoke.py against its own isolated regtest node."""
import hashlib
import json
import socket
import subprocess
import time
from decimal import Decimal


class Electrum:
    def __init__(self, port):
        self.socket = socket.create_connection(("127.0.0.1", port), timeout=10)
        self.socket.settimeout(20)
        self.file = self.socket.makefile("rb")
        self.id = 0
        self.notifications = []

    def request(self, method, params=None, fail=False):
        self.id += 1
        self.socket.sendall((json.dumps({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params or []}) + "\n").encode())
        while True:
            raw = self.file.readline()
            assert raw, "Electrum disconnected"
            value = json.loads(raw)
            if value.get("id") == self.id:
                if fail:
                    assert "error" in value, value
                    return value["error"]
                assert "error" not in value, value
                return value["result"]
            self.notifications.append(value)

    def close(self):
        self.file.close()
        self.socket.close()


def wait(predicate, timeout=45):
    deadline = time.monotonic() + timeout
    while True:
        try:
            result = predicate()
            if result:
                return result
        except (AssertionError, OSError):
            pass
        if time.monotonic() > deadline:
            raise AssertionError("timed out waiting for consistent Electrum state")
        time.sleep(0.3)


def proof(txid, branch, position):
    value = bytes.fromhex(txid)[::-1]
    for sibling in branch:
        other = bytes.fromhex(sibling)[::-1]
        combined = other + value if position & 1 else value + other
        value = hashlib.sha256(hashlib.sha256(combined).digest()).digest()
        position //= 2
    return value[::-1].hex()


def test_server(binary, root, cookie, core_port, rpc, address, historical_txid):
    def core(*args):
        result = rpc(*args)
        assert result.returncode == 0, result.stderr
        try:
            return json.loads(result.stdout)
        except json.JSONDecodeError:
            return result.stdout.strip()

    def scripthash(address):
        script = core("getaddressinfo", address)["scriptPubKey"]
        return hashlib.sha256(bytes.fromhex(script)).digest()[::-1].hex()

    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    with (root / "electrum.log").open("w") as log:
        process = subprocess.Popen([
            binary, "--network", "regtest", "--data-dir", str(root / "archive"), "serve",
            "--cookie-file", str(cookie), "--rpc-url", f"http://127.0.0.1:{core_port}/",
            "--listen", f"127.0.0.1:{port}", "--poll-seconds", "1", "--min-free-mib", "8",
        ], stdout=log, stderr=log)
        client = None
        try:
            client = wait(lambda: Electrum(port))
            wait(lambda: client.request("server.health")["ready"])
            snapshot = client.request("server.health")
            assert snapshot["status"] == "ready" and snapshot["historical_blocks_missing"] is False
            assert isinstance(snapshot["core_pruned"], bool) and snapshot["core_observation_age_seconds"] is not None
            monitor = subprocess.Popen([binary,"--network","regtest","monitor","--address",f"127.0.0.1:{port}","--interval","1"],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
            try:
                first = json.loads(monitor.stdout.readline())
                second = json.loads(monitor.stdout.readline())
                assert first["status"] == "ready" and second["blocks_per_second_since_last_sample"] == 0
            finally:
                monitor.terminate()
                monitor.communicate(timeout=10)
            health = subprocess.run([binary,"--network","regtest","health","--address",f"127.0.0.1:{port}"],capture_output=True,text=True,timeout=90)
            assert health.returncode == 0 and json.loads(health.stdout)["ready"],health.stderr
            wrong = subprocess.run([binary,"--network","mainnet","health","--address",f"127.0.0.1:{port}"],capture_output=True,text=True,timeout=90)
            assert wrong.returncode != 0
            assert client.request("server.version", ["integration", ["1.4", "1.4.2"]])[1] == "1.4"
            features = client.request("server.features")
            assert features["genesis_hash"] == core("getblockhash", "0")
            tip = client.request("blockchain.headers.subscribe")
            assert tip["height"] == core("getblockcount")
            headers = client.request("blockchain.block.headers", {"start_height": 0, "count": 3})
            assert headers["count"] == 3 and len(headers["hex"]) == 480
            cp = client.request("blockchain.block.header", [1, 2])
            assert proof(core("getblockhash", "1"), cp["branch"], 1) == cp["root"]
            raw = client.request("blockchain.transaction.get", [historical_txid])
            assert raw == core("gettransaction", historical_txid)["hex"]
            verbose = client.request("blockchain.transaction.get", [historical_txid, True])
            assert verbose["txid"] == historical_txid and verbose["confirmations"] >= 1
            h = core("getblockheader", core("gettransaction", historical_txid)["blockhash"])["height"]
            merkle = client.request("blockchain.transaction.get_merkle", [historical_txid, h])
            expected = core("getblockheader", core("getblockhash", str(h)))["merkleroot"]
            assert proof(historical_txid, merkle["merkle"], merkle["pos"]) == expected
            positioned = client.request("blockchain.transaction.id_from_pos", [h, merkle["pos"], True])
            assert positioned["tx_hash"] == historical_txid and positioned["merkle"] == merkle["merkle"]
            assert client.request("blockchain.relayfee") >= 0
            assert isinstance(client.request("blockchain.estimatefee", [6]), (int, float))
            genesis = core("getblock", core("getblockhash", "0"), "2")
            script = genesis["tx"][0]["vout"][0]["scriptPubKey"]["hex"]
            genesis_hash = hashlib.sha256(bytes.fromhex(script)).digest()[::-1].hex()
            assert client.request("blockchain.scripthash.get_history", [genesis_hash]) == []
            source_hash = scripthash(address)
            scan = core("scantxoutset", "start", json.dumps([f"addr({address})"]))
            expected_sats = sum(int(Decimal(str(row["amount"])) * 100_000_000) for row in scan["unspents"])
            assert client.request("blockchain.scripthash.get_balance", [source_hash])["confirmed"] == expected_sats

            receiver = core("getnewaddress")
            receiver_hash = scripthash(receiver)
            assert client.request("blockchain.scripthash.subscribe", [receiver_hash]) is None
            parent = core("sendtoaddress", receiver, "0.1")
            wait(lambda: client.request("blockchain.scripthash.get_balance", [receiver_hash])["unconfirmed"] == 10_000_000)
            pending = client.request("blockchain.scripthash.get_mempool", [receiver_hash])
            assert pending[0]["tx_hash"] == parent and pending[0]["height"] == 0 and pending[0]["fee"] > 0
            unspent = client.request("blockchain.scripthash.listunspent", [receiver_hash])
            assert unspent[0]["height"] == 0 and unspent[0]["value"] == 10_000_000
            assert client.request("mempool.get_fee_histogram")
            destination = core("getnewaddress")
            inputs = json.dumps([{"txid": parent, "vout": unspent[0]["tx_pos"]}])
            child_raw = core("createrawtransaction", inputs, json.dumps({destination: 0.0999}), "0", "true")
            child_raw = core("signrawtransactionwithwallet", child_raw)["hex"]
            child = client.request("blockchain.transaction.broadcast", [child_raw])
            wait(lambda: any(p["tx_hash"] == child and p["height"] == -1 for p in client.request("blockchain.scripthash.get_mempool", [receiver_hash])))
            assert client.request("blockchain.scripthash.get_balance", [receiver_hash])["unconfirmed"] == 0
            assert client.request("blockchain.scripthash.listunspent", [receiver_hash]) == []
            replacement_raw = core("createrawtransaction", inputs, json.dumps({destination: 0.0997}), "0", "true")
            replacement_raw = core("signrawtransactionwithwallet", replacement_raw)["hex"]
            replacement = client.request("blockchain.transaction.broadcast", [replacement_raw])
            wait(lambda: any(p["tx_hash"] == replacement for p in client.request("blockchain.scripthash.get_mempool", [receiver_hash])))
            pending = client.request("blockchain.scripthash.get_mempool", [receiver_hash])
            assert child not in [p["tx_hash"] for p in pending]
            block = core("generatetoaddress", "1", address)[0]
            wait(lambda: client.request("server.health")["indexed_tip"]["height"] == 108)
            wait(lambda: all(p["height"] == 108 for p in client.request("blockchain.scripthash.get_history", [receiver_hash])))
            assert client.request("blockchain.scripthash.get_mempool", [receiver_hash]) == []
            core("invalidateblock", block)
            wait(lambda: client.request("server.health")["indexed_tip"]["height"] == 107)
            wait(lambda: any(p["height"] == -1 for p in client.request("blockchain.scripthash.get_history", [receiver_hash])))
            core("setmocktime", str(int(time.time()) + 60))
            core("generatetoaddress", "2", address)
            wait(lambda: client.request("server.health")["indexed_tip"]["height"] == 109)
            wait(lambda: client.request("server.ping") is None and any(n.get("method") == "blockchain.scripthash.subscribe" for n in client.notifications))
            wait(lambda: client.request("server.ping") is None and any(n.get("method") == "blockchain.headers.subscribe" for n in client.notifications))
            assert client.request("blockchain.scripthash.unsubscribe", [receiver_hash]) is True
            assert client.request("blockchain.scripthash.unsubscribe", [receiver_hash]) is False
            assert client.request("does.not.exist", fail=True)["code"] == -32601
            assert client.request("server.version", ["integration", "9.0"], fail=True)["code"] == 1
            client.socket.sendall(b'{bad JSON}\n')
            response = json.loads(client.file.readline())
            while "method" in response:
                response = json.loads(client.file.readline())
            assert response["error"]["code"] == -32700
            client.socket.sendall(b'[{"id":501,"method":"server.ping"},{"id":502,"method":"server.features"}]\n')
            response = json.loads(client.file.readline())
            while not isinstance(response, list):
                response = json.loads(client.file.readline())
            assert [r["id"] for r in response] == [501, 502]
            print("PASS Electrum TCP: negotiation, headers/checkpoints, historical TX/Merkle, balances, mempool chains/RBF, subscriptions, reorg, batches and errors")
        finally:
            if client:
                client.close()
            process.terminate()
            try:
                code = process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
                raise
            assert code == 0, (root / "electrum.log").read_text()
            output = (root / "electrum.log").read_text()
            assert "verificando archivo e índice" in output and "Estado:" in output
            assert "Core:" in output and "Electrum Lite:" in output
            assert cookie.read_text().strip() not in output
