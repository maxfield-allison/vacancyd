#!/usr/bin/env python3
"""Exercise an exact Linux archive on an explicitly disposable systemd host.

Refuses existing Vacancyd installation paths. This changes only its newly created
service/config/binary, retains evidence outside those paths and removes them in a
finally block. It does not test NVIDIA hardware or native desktop presence.
"""
from __future__ import annotations

import argparse
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import platform
import shutil
import socket
import subprocess
import tarfile
import tempfile
import threading
import time
import urllib.error
import urllib.request

INSTALL = Path('/usr/local/bin/vacancyd')
CONFIG = Path('/etc/vacancyd')
UNIT = Path('/etc/systemd/system/vacancyd.service')
STATE = Path('/var/lib/vacancyd')
RUNTIME = Path('/run/vacancyd')
ADMIN_SOCKET = RUNTIME / 'admin.sock'


class Fixture(BaseHTTPRequestHandler):
    admitted = 0

    def log_message(self, *args):
        pass

    def do_GET(self):
        self.respond({'object': 'list', 'data': []})

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
        if self.path == '/v1/chat/completions' and json.loads(body).get('model') == 'qualification-slow':
            # Still generating when the daemon is killed.
            time.sleep(60)
            self.respond({'id': 'qualification-slow', 'choices': []})
            return
        if self.path != '/v1/chat/completions' or json.loads(body).get('model') != 'qualification':
            self.send_error(400)
            return
        type(self).admitted += 1
        self.respond({'id': 'qualification', 'choices': [{'message': {'content': 'accepted'}}]})

    def respond(self, value):
        data = json.dumps(value).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def run(*args):
    return subprocess.run(args, check=True, text=True, capture_output=True).stdout.strip()


def request(url, data=None, headers=None, timeout=3):
    body = None if data is None else json.dumps(data).encode()
    request_headers = dict(headers or {})
    if body is not None:
        request_headers.setdefault('Content-Type', 'application/json')
    req = urllib.request.Request(url, data=body, headers=request_headers)
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    try:
        with opener.open(req, timeout=timeout) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def await_ready():
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        try:
            if request('http://127.0.0.1:9811/readyz')[0] == 200:
                return
        except OSError:
            # The service may not be listening yet; the deadline below still fails readiness.
            pass
        time.sleep(.2)
    raise RuntimeError('installed service never became ready')


def await_vacancy():
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        status = json.loads(request('http://127.0.0.1:9811/status')[1])
        if not status['work'] and all(item['vacancy'] for item in status['upstreams']):
            return status
        time.sleep(.1)
    raise RuntimeError('fresh resource observations did not restore vacancy')


def await_admin_socket():
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if ADMIN_SOCKET.is_socket():
            return
        time.sleep(.1)
    raise RuntimeError('admin socket never appeared')


def admin_request(directory, action):
    """Send one request over the admin socket as root; return its exit code."""
    request = directory / 'admin-request.json'
    request.write_text(json.dumps({'protocol_version': 1, 'action': action}))
    return subprocess.run([str(INSTALL), 'admin-request', '--request', str(request)],
                          text=True, capture_output=True).returncode


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--archive', required=True, type=Path)
    parser.add_argument('--evidence', required=True, type=Path)
    parser.add_argument('--disposable-host-confirmed', required=True, action='store_true')
    args = parser.parse_args()
    if os.geteuid() != 0 or platform.system() != 'Linux' or not Path('/run/systemd/system').exists():
        raise RuntimeError('requires root on the confirmed disposable Linux systemd guest')
    if args.evidence.exists():
        raise RuntimeError('evidence directory must be new')
    for path in (INSTALL, CONFIG, UNIT, STATE, RUNTIME):
        if path.exists() or path.is_symlink():
            raise RuntimeError(f'refusing existing installation path: {path}')
    for port in (9811, 18081):
        with socket.socket() as probe:
            probe.bind(('127.0.0.1', port))
    archive_hash = hashlib.sha256(args.archive.read_bytes()).hexdigest()
    sidecar = args.archive.with_name(args.archive.name + '.sha256').read_text()
    if sidecar != f'{archive_hash}  {args.archive.name}\n':
        raise RuntimeError('archive checksum mismatch')
    expected_target = {'x86_64': 'x86_64', 'aarch64': 'aarch64'}.get(platform.machine())
    if not expected_target or not args.archive.name.endswith(f'-{expected_target}-unknown-linux-gnu.tar.gz'):
        raise RuntimeError('archive target differs from native host architecture')
    args.evidence.mkdir(parents=True)
    receipt = {'archive': args.archive.name, 'sha256': archive_hash,
               'architecture': platform.machine(), 'kernel': platform.release(),
               'scope': 'systemd lifecycle and CPU/memory admission; no GPU or desktop-presence proof',
               'passed': False, 'cleanup_passed': False}
    server = ThreadingHTTPServer(('127.0.0.1', 18080), Fixture)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    created = False
    try:
        with tempfile.TemporaryDirectory(prefix='vacancyd-qualified-') as directory:
            root = Path(directory)
            with tarfile.open(args.archive, 'r:gz') as archive:
                members = archive.getmembers()
                if any(not member.isfile() for member in members):
                    raise RuntimeError('archive contains non-regular payload')
                archive.extractall(root, filter='data')
            payloads = list(root.iterdir())
            if len(payloads) != 1 or not payloads[0].is_dir():
                raise RuntimeError('expected one release payload root')
            source = payloads[0]
            receipt['binary_version'] = run(str(source / 'vacancyd'), '--version')
            created = True
            shutil.copy2(source / 'vacancyd', INSTALL)
            INSTALL.chmod(0o755)
            CONFIG.mkdir(mode=0o700)
            config = '''[server]
listen = "127.0.0.1:9811"
[poll]
interval_ms = 200
[owner]
release_after_s = 0
[owner.gpu_pressure]
enabled = false
[[resource]]
id = "memory"
kind = "system-memory"
reserve = 16777216
[[resource]]
id = "cpu"
kind = "system-cpu"
reserve = 99
[[upstream]]
id = "qualification"
listen = "127.0.0.1:18081"
target = "http://127.0.0.1:18080"
kind = "openai"
resource = "memory"
also_requires = ["cpu"]
default_min_free_mib = 1
'''
            (CONFIG / 'vacancyd.toml').write_text(config)
            (CONFIG / 'vacancyd.toml').chmod(0o600)
            shutil.copy2(source / 'contrib/vacancyd.service', UNIT)
            run(str(INSTALL), 'check-config', '--config', str(CONFIG / 'vacancyd.toml'))
            run('systemd-analyze', 'verify', str(UNIT))
            run('systemctl', 'daemon-reload')
            run('systemctl', 'enable', '--now', 'vacancyd.service')
            await_ready()
            await_admin_socket()
            runtime, socket_file = RUNTIME.lstat(), ADMIN_SOCKET.lstat()
            if (runtime.st_uid, runtime.st_mode & 0o7777) != (0, 0o755) or (socket_file.st_uid, socket_file.st_mode & 0o7777) != (0, 0o666):
                raise RuntimeError('admin socket directory or socket has unexpected ownership or mode')
            receipt['admin_socket_permissions'] = True
            before = json.loads(request('http://127.0.0.1:9811/status')[1])
            (args.evidence / 'status-before.json').write_text(json.dumps(before, indent=2) + '\n')
            work = {'model': 'qualification', 'messages': [{'role': 'user', 'content': 'test'}]}
            status, body = request('http://127.0.0.1:18081/v1/chat/completions', work)
            if status != 200 or json.loads(body).get('id') != 'qualification':
                raise RuntimeError('real admitted request did not reach upstream')
            headers = {'X-Vacancyd-Admin': '1'}
            if request('http://127.0.0.1:9811/admin/lending/disable', {}, headers)[0] != 200:
                raise RuntimeError('lending disable failed')
            count = Fixture.admitted
            if request('http://127.0.0.1:18081/v1/chat/completions', work)[0] != 503 or Fixture.admitted != count:
                raise RuntimeError('disabled lending reached upstream')
            run('systemctl', 'restart', 'vacancyd.service')
            await_ready()
            if request('http://127.0.0.1:18081/v1/chat/completions', work)[0] != 200:
                raise RuntimeError('restart did not recover admitted requests')
            await_vacancy()
            # Work still being forwarded when the daemon dies has no verified
            # completion, so after the restart it must come back held.
            def slow_request():
                try:
                    request('http://127.0.0.1:18081/v1/chat/completions',
                            dict(work, model='qualification-slow'), timeout=90)
                except OSError:
                    # This fixture connection is deliberately interrupted by the service restart.
                    pass
            threading.Thread(target=slow_request, daemon=True).start()
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                in_flight = json.loads(request('http://127.0.0.1:9811/status')[1])['work']
                if len(in_flight) == 1 and in_flight[0]['phase'] == 'forwarding':
                    break
                time.sleep(.1)
            else:
                raise RuntimeError('slow request never reached forwarding')
            killed = run('systemctl', 'show', '-p', 'MainPID', '--value', 'vacancyd.service')
            os.kill(int(killed), 9)
            deadline = time.monotonic() + 30
            while run('systemctl', 'show', '-p', 'MainPID', '--value', 'vacancyd.service') in ('0', killed):
                if time.monotonic() > deadline:
                    raise RuntimeError('service was not restarted after SIGKILL')
                time.sleep(.2)
            await_ready()
            held = json.loads(request('http://127.0.0.1:9811/status')[1])
            if len(held['work']) != 1 or held['work'][0]['phase'] != 'held':
                raise RuntimeError('work interrupted by a crash did not return held')
            work_id = held['work'][0]['id']
            count = Fixture.admitted
            if request('http://127.0.0.1:18081/v1/chat/completions', work)[0] != 503 or Fixture.admitted != count:
                raise RuntimeError('held work allowed another upstream request')
            journal = STATE / 'admission'
            records = list(journal.glob('entry-*.json'))
            if len(records) != 1 or journal.stat().st_mode & 0o077 or records[0].stat().st_mode & 0o077:
                raise RuntimeError('journal is missing or has public permissions')
            receipt['journal_permissions_private'] = True
            run('systemctl', 'restart', 'vacancyd.service')
            await_ready()
            recovered = json.loads(request('http://127.0.0.1:9811/status')[1])
            (args.evidence / 'status-held-after-restart.json').write_text(json.dumps(recovered, indent=2) + '\n')
            if len(recovered['work']) != 1 or recovered['work'][0]['id'] != work_id:
                raise RuntimeError('restart lost the unresolved work record')
            if request('http://127.0.0.1:18081/v1/chat/completions', work)[0] != 503 or Fixture.admitted != count:
                raise RuntimeError('restart silently released unresolved work')
            release_url = f'http://127.0.0.1:9811/admin/work/{work_id}/release'
            if request(release_url, {'upstream_stopped': True})[0] != 403:
                raise RuntimeError('recovery accepted missing administrator intent')
            status, body = request(release_url, {'upstream_stopped': True}, headers)
            if status != 403 or json.loads(body)['error']['type'] != 'administrator_required':
                raise RuntimeError('loopback without a token released held work')
            receipt['loopback_admin_refused'] = True
            await_admin_socket()
            if admin_request(root, {'kind': 'release_work', 'id': work_id, 'upstream_stopped': False}) != 22:
                raise RuntimeError('recovery accepted a false upstream-stop confirmation')
            if admin_request(root, {'kind': 'release_work', 'id': work_id, 'upstream_stopped': True}) != 0:
                raise RuntimeError('confirmed administrator recovery failed')
            await_vacancy()
            if list(journal.glob('entry-*.json')):
                raise RuntimeError('administrator recovery left an unresolved journal record')
            if request('http://127.0.0.1:18081/v1/chat/completions', work)[0] != 200:
                raise RuntimeError('fresh observations did not permit work after recovery')
            receipt['restart_retained_uncertain_work'] = True
            receipt['guarded_manual_recovery_passed'] = True
            receipt['accepted_requests'] = Fixture.admitted
            receipt['passed'] = True
    finally:
        server.shutdown()
        server.server_close()
        try:
            if created:
                result = subprocess.run(['journalctl', '-u', 'vacancyd.service', '--no-pager', '-n', '200'],
                                        text=True, capture_output=True)
                (args.evidence / 'journal.txt').write_text(result.stdout)
                if UNIT.exists():
                    run('systemctl', 'disable', '--now', 'vacancyd.service')
                    if subprocess.run(['systemctl', 'is-active', '--quiet', 'vacancyd.service']).returncode == 0:
                        raise RuntimeError('service remained active; installation preserved for diagnosis')
                for path in (INSTALL, UNIT):
                    path.unlink(missing_ok=True)
                if RUNTIME.exists():
                    raise RuntimeError('stopping the service left its runtime directory')
                for path in (CONFIG, STATE):
                    if path.exists():
                        shutil.rmtree(path)
                run('systemctl', 'daemon-reload')
                receipt['cleanup_passed'] = all(not path.exists() for path in (INSTALL, CONFIG, UNIT, STATE, RUNTIME))
        except Exception as error:
            receipt['cleanup_error'] = str(error)
        finally:
            (args.evidence / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    if not receipt['passed'] or not receipt['cleanup_passed']:
        raise RuntimeError('qualification or cleanup failed')
    print(json.dumps(receipt, indent=2))


if __name__ == '__main__':
    main()
