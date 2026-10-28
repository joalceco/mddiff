"""Private loopback HTTPS fixture for main-server API and real Codex MCP checks."""

from contextlib import ExitStack, contextmanager
from http.client import HTTPConnection, HTTPException
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import select
import signal
import socket
import ssl
import subprocess
import tempfile
import threading
from types import SimpleNamespace
from urllib.error import HTTPError
from urllib.parse import urlencode, urlsplit
from urllib.request import HTTPSHandler, HTTPCookieProcessor, ProxyHandler, Request, build_opener

from test_hosted_auth import Browser, CheckFailed, Redirects, ROOT, check, fixture as idp, load_runtime, web_client


LIMIT = 24 * 1024 * 1024 + 16 * 1024


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


class TlsBrowser(Browser):
    def __init__(self, base, issuer, trust):
        super().__init__(base, issuer)
        allowed = [(urlsplit(url).scheme, urlsplit(url).hostname, urlsplit(url).port) for url in (base, issuer)]
        self.redirects = Redirects(allowed)
        self.opener = build_opener(ProxyHandler({}), HTTPCookieProcessor(self.cookies), HTTPSHandler(context=trust), self.redirects)

    def cookie(self):
        cookies = [cookie for cookie in self.cookies if cookie.name == "__Host-mddiff-session"]
        check(len(cookies) == 1, "HTTPS viewer did not set its host-prefixed session cookie.")
        check(cookies[0].secure and cookies[0].path == "/" and not cookies[0].domain_specified, "HTTPS viewer weakened its session cookie.")
        attributes = {name.lower(): value for name, value in cookies[0]._rest.items()}
        check("httponly" in attributes and str(attributes.get("samesite", "")).lower() == "lax", "HTTPS viewer weakened its cookie attributes.")
        return cookies[0].value


def http(env, path, body=None, access=None, method=None, headers=None, opener=None):
    outgoing = {**({"Authorization": "Bearer " + access} if access else {}), **(headers or {})}
    if body is not None:
        outgoing["Content-Type"] = "application/json"
    request = Request(env.base + path, data=None if body is None else json.dumps(body, ensure_ascii=False).encode(), headers=outgoing, method=method)
    client = opener or build_opener(ProxyHandler({}), HTTPSHandler(context=env.trust), Redirects())
    try:
        response = client.open(request, timeout=15)
    except HTTPError as error:
        response = error
    with response:
        return response.status, response.read(LIMIT * 3), response.headers


def token(env, user="alice", client="mddiff-test", scope="openid profile mddiff:compare", foreign=False):
    return idp.request(env.runtime["issuer"] + ("-foreign" if foreign else "") + "/protocol/openid-connect/token", {
        "grant_type": "password", "client_id": client, "scope": scope,
        "username": env.runtime["users"][user]["username"], "password": env.runtime["users"][user]["password"],
    }, form=True)["access_token"]


def certificates(directory):
    ca, key, cert = directory / "ca.pem", directory / "server.key", directory / "server.pem"
    configuration = directory / "ca.cnf"
    configuration.write_text("[req]\nprompt=no\ndistinguished_name=dn\nx509_extensions=ca\n[dn]\nCN=mddiff synthetic test CA\n[ca]\nbasicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\nsubjectKeyIdentifier=hash\n")
    commands = [
        ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-sha256",
         "-config", str(configuration), "-keyout", str(directory / "ca.key"), "-out", str(ca)],
        ["openssl", "req", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=127.0.0.1", "-keyout", str(key), "-out", str(directory / "server.csr")],
    ]
    for command in commands:
        subprocess.run(command, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    extensions = directory / "server.ext"
    extensions.write_text("subjectAltName=IP:127.0.0.1\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")
    subprocess.run(["openssl", "x509", "-req", "-days", "1", "-sha256", "-in", str(directory / "server.csr"),
                    "-CA", str(ca), "-CAkey", str(directory / "ca.key"), "-CAcreateserial", "-extfile", str(extensions), "-out", str(cert)],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for path in directory.iterdir():
        path.chmod(0o600)
    return ca, cert, key


@contextmanager
def tls_proxy(directory, backend_port):
    ca, cert, key = certificates(directory)
    hop = {"connection", "proxy-connection", "keep-alive", "transfer-encoding", "upgrade", "te", "trailer"}

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *args):
            pass  # Request targets can contain OAuth codes; never log them.

        def forward(self):
            self.close_connection = True
            lengths = self.headers.get_all("Content-Length", [])
            if self.headers.get("Transfer-Encoding") or len(lengths) > 1:
                self.send_error(400)
                return
            try:
                length = int(lengths[0]) if lengths else 0
            except ValueError:
                self.send_error(400)
                return
            if length < 0 or length > LIMIT:
                self.send_error(413)
                return
            if self.headers.get("Host") != f"127.0.0.1:{self.server.server_port}" or not self.path.startswith("/") or self.path.startswith("//"):
                self.send_error(400)
                return
            connection = HTTPConnection("127.0.0.1", backend_port, timeout=30)
            started = False
            try:
                connection.putrequest(self.command, self.path, skip_host=True, skip_accept_encoding=True)
                for name, value in self.headers.raw_items():
                    if name.lower() not in hop and not name.lower().startswith("x-forwarded-") and name.lower() != "forwarded":
                        connection.putheader(name, value)
                connection.endheaders()
                remaining = length
                while remaining:
                    chunk = self.rfile.read(min(65536, remaining))
                    if not chunk:
                        raise OSError("Synthetic proxy body ended early.")
                    connection.send(chunk)
                    remaining -= len(chunk)
                upstream = connection.getresponse()
                self.send_response(upstream.status)
                for name, value in upstream.getheaders():
                    if name.lower() not in hop:
                        self.send_header(name, value)
                self.send_header("Connection", "close")
                self.end_headers()
                started = True
                if self.command != "HEAD":
                    while chunk := upstream.read1(65536):
                        self.wfile.write(chunk)
                        self.wfile.flush()
            except (OSError, HTTPException):
                if not started:
                    self.send_error(502)
            finally:
                connection.close()

        do_GET = do_HEAD = do_POST = do_PUT = do_PATCH = do_DELETE = do_OPTIONS = forward

    class Server(ThreadingHTTPServer):
        daemon_threads = True
        request_queue_size = 64

        def handle_error(self, *args):
            pass

    server = Server(("127.0.0.1", 0), Handler)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    server.socket = context.wrap_socket(server.socket, server_side=True)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"https://127.0.0.1:{server.server_port}", ca
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


@contextmanager
def audience_clients(runtime, resource):
    changed = []
    admin = idp.admin_token(runtime)
    try:
        for foreign, names in [(False, ["mddiff-codex", "mddiff-test", "mddiff-expiry-test", "mddiff-wrong-audience"]), (True, ["mddiff-test"])]:
            realm = idp.REALM + ("-foreign" if foreign else "")
            endpoint = idp.BASE + "/admin/realms/" + realm + "/clients"
            for name in names:
                matches = idp.request(endpoint + "?" + urlencode({"clientId": name}), token=admin)
                check(len(matches) == 1, "Synthetic OAuth client is missing.")
                url = endpoint + "/" + matches[0]["id"]
                original = idp.request(url, token=admin)
                changed.append((url, original))
                updated = json.loads(json.dumps(original))
                updated["protocolMappers"] = [mapper for mapper in updated.get("protocolMappers", []) if mapper.get("name") != "mcp-audience"]
                if name != "mddiff-wrong-audience":
                    updated["protocolMappers"].append(idp.mapper("mcp-audience", "included.custom.audience", resource))
                idp.request(url, updated, admin, "PUT")
        yield
    finally:
        for url, original in reversed(changed):
            idp.request(url, original, idp.admin_token(runtime), "PUT")


@contextmanager
def hosted_fixture():
    runtime = load_runtime(idp.RUNTIME)
    check(runtime["issuer"] == "http://localhost:8848/realms/mddiff-spike", "Only the isolated synthetic issuer is allowed.")
    binary = ROOT / "target/release/mddiff"
    check(binary.is_file(), "Compile the main release binary first.")
    with ExitStack() as stack:
        directory = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix="mddiff-https-mcp-")))
        backend_port = free_port()
        base, ca = stack.enter_context(tls_proxy(directory, backend_port))
        resource = base + "/mcp"
        stack.enter_context(audience_clients(runtime, resource))
        stack.enter_context(web_client(runtime, base))
        config_path = directory / "oidc.json"
        (directory / "data").mkdir(mode=0o700)
        idp.private_write(config_path, json.dumps({
            "issuer": runtime["issuer"], "public_origin": base, "client_id": "mddiff-hosted-web",
            "introspection_client_id": "mddiff-resource", "introspection_client_secret": runtime["resource_client"]["secret"],
            "audience": "mddiff-resource", "allow_insecure_loopback": True,
        }))
        process = subprocess.Popen([str(binary), "--mode", "hosted", "--port", str(backend_port), "--oidc-config", str(config_path), "--data-dir", str(directory / "data")],
                                   cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            check(bool(select.select([process.stdout], [], [], 10)[0]), "Main HTTPS backend did not start.")
            check(process.stdout.readline().strip() == f"MDdiff: http://127.0.0.1:{backend_port}", "Unexpected main backend startup.")
            trust = ssl.create_default_context(cafile=str(ca))
            yield SimpleNamespace(base=base, resource=resource, ca=ca, trust=trust, runtime=runtime, directory=directory)
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGINT)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
                raise CheckFailed("Main HTTPS backend did not stop.") from None
            process.stdout.close()
            process.stderr.close()
