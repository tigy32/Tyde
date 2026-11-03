import os, sys, json, subprocess, tempfile, shutil, socket, threading, pwd, queue, signal, shlex
from pathlib import Path

assert (
    len(sys.argv) == 3
), "usage: repro_pending_ssh_cancellation.py NEW_CASE_DIR CONTROLLER"
E = Path(sys.argv[1]).resolve()
E.mkdir(exist_ok=False)
CONTROLLER = Path(sys.argv[2]).resolve()
SOURCE = Path(__file__).resolve().parent.parent
import urllib.request

BASE = E


class Client:
    def __init__(self):
        att = BASE / "host-state/debug-mcp.json"
        assert att.stat().st_mode & 0o777 == 0o600
        descriptor = json.loads(att.read_text())["servers"][0]
        assert (
            descriptor["name"] == "tyde-debug"
            and descriptor["transport"]["headers"] == {}
        )
        self.url = descriptor["transport"]["url"]
        assert self.url.startswith("http://127.0.0.1:")
        self.seq = 0

    def call(self, name, args, label):
        self.seq += 1
        path = BASE / (label + ".json")
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        try:
            request = {
                "jsonrpc": "2.0",
                "id": self.seq,
                "method": "tools/call",
                "params": {"name": name, "arguments": args},
            }
            req = urllib.request.Request(
                self.url,
                json.dumps(request).encode(),
                {
                    "Content-Type": "application/json",
                    "Accept": "application/json, text/event-stream",
                },
            )
            with urllib.request.urlopen(req) as response:
                raw = response.read().decode()
                kind = response.headers.get_content_type()
            if kind == "text/event-stream":
                values = [
                    json.loads(line[5:])
                    for line in raw.splitlines()
                    if line.startswith("data:")
                ]
                obj = next(v for v in values if v.get("id") == self.seq)
            else:
                obj = json.loads(raw)
            if "error" in obj:
                raise RuntimeError("canonical MCP RPC failed")
            value = obj["result"]
            with os.fdopen(fd, "w") as f:
                fd = None
                json.dump(value, f, indent=2)
                f.write("\n")
            if value.get("isError"):
                raise RuntimeError("canonical MCP error retained in " + str(path))
            assert len(value["content"]) == 1 and value["content"][0]["type"] == "text"
            return json.loads(value["content"][0]["text"])
        except BaseException as error:
            if fd is not None:
                os.write(
                    fd,
                    (
                        json.dumps({"requestFailedClass": type(error).__name__}) + "\n"
                    ).encode(),
                )
            raise
        finally:
            if fd is not None:
                os.close(fd)


class Observation:
    def __init__(self, stage):
        self.c = Client()
        self.id = json.loads((E / "HANDLE.json").read_text())["instanceId"]
        self.n = 0
        self.stage = stage

    def ev(self, s):
        self.n += 1
        r = self.c.call(
            "tyde_debug_evaluate",
            {
                "instance_id": self.id,
                "expression": "try{"
                + s
                + "}catch(e){return {observationError:String(e),message:e?.message??null};}",
                "timeout_ms": 600000,
            },
            f"{self.stage}-{self.n:02d}",
        )
        if isinstance(r["value"], dict) and "observationError" in r["value"]:
            raise RuntimeError(json.dumps(r["value"]))
        return r["value"]


children = []
threads = []
gate = None
native = None
instance_stopped = False
gate_thread = None
gs = None
import ctypes

assert ctypes.CDLL(None).prctl(36, 1, 0, 0, 0) == 0, "Cannot own fixture descendants"
try:
    for name in ["hostkey", "unauthorized"]:
        subprocess.run(
            ["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(E / name)],
            check=True,
        )
    gs = socket.socket(socket.AF_UNIX)
    gs.bind(str(E / "auth.sock"))
    gs.listen(1)
    port = 49437
    native = Path(tempfile.mkdtemp(prefix="tyde-auth-"))
    bridge = shlex.join(
        [
            "env",
            "TYDE_SOCKET_PATH=" + str(native / "host.sock"),
            str(CONTROLLER),
            "host",
            "--bridge-uds",
        ]
    )
    conf = f"""ListenAddress 127.0.0.1
Port {port}
HostKey {E}/hostkey
PidFile {E}/sshd.pid
UsePAM no
PerSourcePenalties no
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
AuthenticationMethods publickey
AuthorizedKeysFile {E}/authorized_keys
ForceCommand {bridge}
LogLevel DEBUG1
"""
    (E / "sshd.conf").write_text(conf)
    (E / "authorized_keys").write_text("")
    sshd = subprocess.Popen(
        [
            "/usr/bin/python3",
            str(SOURCE / "tools/ssh_auth_file_gate.py"),
            str(E / "sshd.conf"),
            str(E / "authorized_keys"),
            str(E / "auth.sock"),
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    children.append(sshd)
    with (E / "sshd.log").open("x") as f:
        for line in sshd.stderr:
            f.write(line)
            f.flush()
            if "Server listening on " in line:
                break
        else:
            raise RuntimeError("Owned sshd failed before listening")

    def sshlog():
        with (E / "sshd.log").open("a") as f:
            for line in sshd.stderr:
                f.write(line)
                f.flush()

    t = threading.Thread(target=sshlog)
    t.start()
    threads.append(t)
    (E / "known_hosts").write_text(
        f"[127.0.0.1]:{port} " + (E / "hostkey.pub").read_text()
    )
    (E / "client.conf").write_text(f"""Host tyde-owned-auth
 HostName 127.0.0.1
 Port {port}
 User {pwd.getpwuid(os.getuid()).pw_name}
 IdentityFile {E}/unauthorized
 IdentityAgent none
 IdentitiesOnly yes
 UserKnownHostsFile {E}/known_hosts
 GlobalKnownHostsFile /dev/null
 StrictHostKeyChecking yes
 BatchMode yes
 PreferredAuthentications publickey
 PasswordAuthentication no
 KbdInteractiveAuthentication no
 ControlMaster no
 ControlPath none
""")
    r, w = os.pipe()
    x = subprocess.Popen(
        [
            "Xvfb",
            "-displayfd",
            str(w),
            "-screen",
            "0",
            "1440x1000x24",
            "-nolisten",
            "tcp",
        ],
        pass_fds=(w,),
        stdout=subprocess.DEVNULL,
        stderr=(E / "display.stderr").open("x"),
    )
    children.append(x)
    os.close(w)
    with os.fdopen(r) as f:
        display = ":" + f.readline().strip()
    assert display != ":"
    state = E / "host-state"
    state.mkdir()
    env = os.environ.copy()
    env.update(
        DISPLAY=display,
        RUST_LOG="info,tauri_shell::host_io=debug",
        TYDE_SOCKET_PATH=str(native / "host.sock"),
        TYDE_DEBUG_MCP_ATTACHMENT_PATH=str(state / "debug-mcp.json"),
        TYDE_HOST_LOG_PATH=str(state / "host.log"),
    )
    for key, name in {
        "TYDE_SESSION_STORE_PATH": "sessions.json",
        "TYDE_PROJECT_STORE_PATH": "projects.json",
        "TYDE_SETTINGS_STORE_PATH": "settings.json",
        "TYDE_AGENT_TEAMS_STORE_PATH": "agent_teams.json",
        "TYDE_REVIEW_STORE_PATH": "reviews.json",
        "TYDE_AGENTS_VIEW_PREFERENCES_STORE_PATH": "agents_view_preferences.json",
        "TYDE_CUSTOM_AGENTS_STORE_PATH": "custom_agents.json",
        "TYDE_MCP_SERVERS_STORE_PATH": "mcp_servers.json",
        "TYDE_STEERING_STORE_PATH": "steering.json",
        "TYDE_SKILLS_STORE_PATH": "skills.json",
        "TYDE_SKILLS_DIR_PATH": "skills",
        "TYDE_MOBILE_PAIRINGS_STORE_PATH": "mobile_pairings.json",
        "TYDE_WORKFLOW_RUN_STORE_PATH": "workflow_runs.json",
        "TYDE_GLOBAL_WORKFLOWS_DIR": "global_workflows",
        "TYDE_CONFIGURED_HOST_STORE_PATH": "configured_hosts.json",
        "TYDE_TRACING_DIR_PATH": "tracing",
    }.items():
        env[key] = str(state / name)
    h = subprocess.Popen(
        [str(CONTROLLER), "host", "--uds"],
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=(E / "host.stderr").open("x"),
        text=True,
    )
    children.append(h)
    first = h.stdout.readline()
    (E / "controller-first-line.json").write_text(first)
    if json.loads(first) != {"debug_mcp_attachment": str(state / "debug-mcp.json")}:
        raise RuntimeError("Controller did not announce attachment")
    (E / "setup.json").write_text(
        json.dumps(
            {
                "display": display,
                "sshPort": port,
                "controllerBinarySha256": subprocess.check_output(
                    ["sha256sum", str(CONTROLLER)], text=True
                ).split()[0],
            }
        )
    )
    c = Client()
    started = c.call("tyde_dev_instance_start", {"project_dir": str(SOURCE)}, "START")
    (E / "HANDLE.json").write_text(json.dumps({"instanceId": started["instanceId"]}))
    ready = c.call(
        "tyde_dev_instance_wait", {"instance_id": started["instanceId"]}, "READY"
    )
    assert ready["status"]["kind"] == "ready" and ready["storesEphemeral"]
    assert all(
        w["dataStore"] == "non_persistent" for w in ready["nativeState"]["webviews"]
    )
    o = Observation("cancel-observe")
    assert o.ev(
        'await window.__TAURI__.core.invoke("configure_dev_instance_ssh",{config:'
        + json.dumps((E / "client.conf").read_text())
        + "});return true;"
    )
    # An actual Welcome completes the inner protocol handshake, so control
    # connections cannot expire while subsequent SSH cases are held.
    # These are protocol fixture inputs (protocol/src/types.rs), not a client
    # status model or a substitute SSH response.
    assert o.ev("""
        window.completeOwnedHandshake=async hostId=>{
            let resolveWelcome,rejectWelcome;
            const welcome=new Promise((resolve,reject)=>{resolveWelcome=resolve;rejectWelcome=reject;});
            const unlisten=await window.__TAURI__.event.listen('tyde://host-line',e=>{
                if(e.payload.hostId!==hostId)return;
                const frame=JSON.parse(e.payload.line);
                if(frame.kind==='welcome')resolveWelcome(true);
                if(frame.kind==='reject')rejectWelcome(Error('Owned Hello rejected'));
            });
            try{
                await window.__TAURI__.core.invoke('send_host_line',{hostId,line:JSON.stringify({stream:'/host/owned-cancellation-regression',seq:0,kind:'hello',payload:{protocol_version:96,tyde_version:{major:0,minor:8,patch:14},client_name:'owned-cancellation-regression',platform:'native-regression'}})});
                return await welcome;
            }finally{unlisten();}
        };
        return true;
    """)
    assert o.ev(
        "await window.__TAURI__.core.invoke('upsert_configured_host',{request:{id:'owned-manual-auth',label:'Owned manual auth',transport:{kind:'ssh_stdio',ssh_destination:'tyde-owned-auth',lifecycle:{kind:'manual'}},auto_connect:false}});return true;"
    )
    assert o.ev(
        "window.nativeConnectPending=true;window.nativeConnectResult=null;window.nativeConnectOriginal=window.__TAURI__.core.invoke('connect_host',{hostId:'owned-manual-auth'});window.nativeConnectCompletion=window.nativeConnectOriginal.then(()=>{window.nativeConnectPending=false;window.nativeConnectResult={ok:true};return window.nativeConnectResult;},e=>{window.nativeConnectPending=false;window.nativeConnectResult={ok:false,error:String(e)};return window.nativeConnectResult;});return true;"
    )
    gate, _ = gs.accept()
    assert gate.recv(64) == b"AUTH_DECISION_ENTERED"
    print("NATIVE_AUTHORIZATION_HELD_FOR_CANCEL_OBSERVATION", flush=True)
    import re, select

    config_path = Path(ready["storeDir"]) / "ssh-observation.conf"
    candidates = []
    for f in (Path(ready["storeDir"]) / "tracing").glob("*"):
        for pid in re.findall(r"child_pid=(\d+)", f.read_text()):
            try:
                argv = (Path("/proc") / pid / "cmdline").read_bytes().split(b"\0")
            except FileNotFoundError:
                continue
            if (
                str(config_path).encode() in argv
                and Path(os.fsdecode(argv[0])).name == "ssh"
            ):
                candidates.append(int(pid))
    candidates = list(set(candidates))
    if len(candidates) != 1:
        raise RuntimeError("Missing unique owned SSH process receipt")
    fd = os.pidfd_open(candidates[0])
    poll = select.poll()
    poll.register(fd, select.POLLIN)
    before = o.ev(
        "return {connectPending:window.nativeConnectPending,result:window.nativeConnectResult};"
    )
    before_exit = bool(poll.poll(0))
    if not before["connectPending"] or before_exit:
        raise RuntimeError(
            "Required pending-auth state was not observed; no cancellation conclusion"
        )
    ack = o.ev(
        "await window.__TAURI__.core.invoke('disconnect_host',{hostId:'owned-manual-auth'});return {ok:true,connectPending:window.nativeConnectPending};"
    )
    after = o.ev(
        "return {connectPending:window.nativeConnectPending,result:window.nativeConnectResult};"
    )
    after_exit = bool(poll.poll(0))
    receipt = {
        "sourceCommit": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=SOURCE, text=True
        ).strip(),
        "scope": "Real production native connect_host/disconnect_host IPC, not the rendered host-connect action; UI lifecycle state is not used as evidence",
        "gate": "real OpenSSH authorization file open, still held",
        "ownedProcessBinding": "unique native launch receipt with exact disposable SSH config argument, held by pidfd",
        "before": before,
        "sshExitedBeforeDisconnect": before_exit,
        "nativeDisconnectAcknowledgment": ack,
        "afterNativeAcknowledgment": after,
        "sshExitedAtAcknowledgment": after_exit,
        "observer": "Original native connect Promise retained directly; no native API wrapper or replacement",
    }
    (E / "CANCEL-AT-ACK.json").write_text(json.dumps(receipt, indent=2))
    # A completed cancellation must reap the exact child before acknowledgment.
    # No auth release, elapsed threshold, or handshake timeout can satisfy this.
    if not after_exit:
        (E / "REGRESSION.json").write_text(
            json.dumps(
                {
                    "result": "RED",
                    "reason": "owned SSH client still alive at disconnect acknowledgment",
                }
            )
        )
        raise AssertionError(
            "Owned SSH client is still alive after disconnect acknowledgment"
        )
    terminal = o.ev("return await window.nativeConnectCompletion;")
    assert terminal == {
        "ok": False,
        "error": "connection attempt cancelled",
    }, "Native connect must settle from cancellation, not handshake expiry/rejection"
    assert not o.ev("return window.nativeConnectPending;")
    gate.sendall(b"R")
    gate.close()
    gate = None
    poll.poll()
    os.close(fd)
    (E / "REGRESSION.json").write_text(
        json.dumps(
            {
                "result": "GREEN",
                "connectResult": terminal,
                "ownedChildExitedAtAcknowledgment": after_exit,
                "authGateHeldAtAcknowledgment": True,
            },
            indent=2,
        )
    )

    # A replacement owns a new generation. Killing its predecessor must neither
    # unregister nor cancel it; a separate host must survive both teardowns.
    assert o.ev(
        "await window.__TAURI__.core.invoke('upsert_configured_host',{request:{id:'owned-other',label:'Owned other',transport:{kind:'local_embedded'},auto_connect:false}});await window.__TAURI__.core.invoke('connect_host',{hostId:'owned-other'});return await window.completeOwnedHandshake('owned-other');"
    )

    def attempt(name):
        return o.ev(
            "window["
            + json.dumps(name)
            + "]={pending:true,result:null};const a=window["
            + json.dumps(name)
            + "];a.original=window.__TAURI__.core.invoke('connect_host',{hostId:'owned-manual-auth'});a.completion=a.original.then(()=>{a.pending=false;return a.result={ok:true};},e=>{a.pending=false;return a.result={ok:false,error:String(e)};});return true;"
        )

    def owned_client():
        candidates = set()
        for f in (Path(ready["storeDir"]) / "tracing").glob("*"):
            for pid in re.findall(r"child_pid=(\d+)", f.read_text()):
                try:
                    argv = (Path("/proc") / pid / "cmdline").read_bytes().split(b"\0")
                except FileNotFoundError:
                    continue
                if (
                    str(config_path).encode() in argv
                    and Path(os.fsdecode(argv[0])).name == "ssh"
                ):
                    candidates.add(int(pid))
        assert len(candidates) == 1, "Missing unique owned SSH client"
        fd = os.pidfd_open(candidates.pop())
        p = select.poll()
        p.register(fd, select.POLLIN)
        return fd, p

    assert attempt("predecessor")
    gate, _ = gs.accept()
    assert gate.recv(64) == b"AUTH_DECISION_ENTERED"
    fd, poll = owned_client()
    assert o.ev("return window.predecessor.pending;") and not poll.poll(
        0
    ), "Required pending predecessor not observed"
    assert attempt("replacement")
    # Completion is the process event, not a timer or an auth release.
    poll.poll()
    os.close(fd)
    assert o.ev("return await window.predecessor.completion;") == {
        "ok": False,
        "error": "connection attempt cancelled",
    }
    gate.sendall(b"R")
    gate.close()
    gate = None
    gate, _ = gs.accept()
    assert gate.recv(64) == b"AUTH_DECISION_ENTERED"
    fd, poll = owned_client()
    assert o.ev("return window.replacement.pending;") and not poll.poll(
        0
    ), "Replacement must remain owned and pending"
    # Two disconnects join one generation's cleanup. Observe both original IPC
    # Promises directly, without replacing the native API.
    ack = o.ev(
        "const a=window.__TAURI__.core.invoke('disconnect_host',{hostId:'owned-manual-auth'});const b=window.__TAURI__.core.invoke('disconnect_host',{hostId:'owned-manual-auth'});await Promise.all([a,b]);return true;"
    )
    assert ack and poll.poll(
        0
    ), "Both disconnect acknowledgments must follow child exit"
    assert o.ev("return await window.replacement.completion;") == {
        "ok": False,
        "error": "connection attempt cancelled",
    }
    assert o.ev(
        "await window.__TAURI__.core.invoke('retry_host',{hostId:'owned-other'});await window.__TAURI__.core.invoke('disconnect_host',{hostId:'owned-other'});return true;"
    ), "Unrelated host must retain its registered connection"
    gate.sendall(b"R")
    gate.close()
    gate = None
    os.close(fd)
    (E / "GENERATIONS.json").write_text(
        json.dumps(
            {
                "supersededAttemptCancelled": True,
                "replacementAliveAtItsAuthGate": True,
                "bothDisconnectsJoinedChildExit": True,
                "unrelatedHostRetained": True,
            },
            indent=2,
        )
    )

    # Complete a genuine SSH/recovery handshake against the owned host, then
    # break only that transport and hold its replacement in authentication.
    (E / "authorized_keys").write_text((E / "unauthorized.pub").read_text())
    events = queue.Queue()

    def auth_events():
        while True:
            try:
                connection, _ = gs.accept()
            except OSError:
                return
            if connection.recv(64) != b"AUTH_DECISION_ENTERED":
                events.put(("error", "Missing authorization event"))
                connection.close()
                return
            events.put(("gate", connection))

    gate_thread = threading.Thread(target=auth_events)
    gate_thread.start()
    assert attempt("recoveryInitial")

    def initial_completion():
        try:
            value = Observation("initial-completion").ev(
                "const result=await window.recoveryInitial.completion;if(result.ok)await window.completeOwnedHandshake('owned-manual-auth');return result;"
            )
            events.put(("complete", value))
        except BaseException as error:
            events.put(("error", type(error).__name__))

    completion_thread = threading.Thread(target=initial_completion)
    completion_thread.start()
    threads.append(completion_thread)
    while True:
        kind, value = events.get()
        if kind == "complete":
            assert value == {
                "ok": True
            }, "Owned real SSH connection must complete recovery negotiation"
            break
        assert kind == "gate", "Initial SSH connection observation failed"
        value.sendall(b"R")
        value.close()
    fd, poll = owned_client()
    assert not poll.poll(0), "Initial authorized SSH client must be alive"
    assert o.ev(
        "window.detached=new Promise(resolve=>{window.detachedResolve=resolve;});window.unlistenRecovery=await window.__TAURI__.event.listen('tyde://host-recovery',e=>{if(e.payload.hostId==='owned-manual-auth'&&!e.payload.connected)window.detachedResolve(true);});return true;"
    )
    signal.pidfd_send_signal(fd, signal.SIGKILL)
    poll.poll()
    os.close(fd)
    assert o.ev(
        "await window.detached;await window.__TAURI__.core.invoke('retry_host',{hostId:'owned-manual-auth'});return true;"
    )
    kind, gate = events.get()
    assert kind == "gate", "Recovery must reach a genuine authorization decision"
    fd, poll = owned_client()
    assert not poll.poll(0), "Required pending reconnect child was not observed"
    assert o.ev(
        "await window.__TAURI__.core.invoke('disconnect_host',{hostId:'owned-manual-auth'});return true;"
    )
    assert poll.poll(
        0
    ), "Disconnect must join the reconnect SSH child before acknowledgment"
    os.close(fd)
    # The registered connection must be gone, not merely marked unavailable.
    assert o.ev(
        "try{await window.__TAURI__.core.invoke('retry_host',{hostId:'owned-manual-auth'});return false;}catch(e){return String(e)==='host needs a fresh connection';}"
    )
    gate.sendall(b"R")
    gate.close()
    gate = None
    (E / "RECONNECT.json").write_text(
        json.dumps(
            {
                "realRecoveryHandshakeCompleted": True,
                "exactInitialChildExited": True,
                "reconnectAuthHeldAtDisconnectAck": True,
                "exactReconnectChildExitedAtAck": True,
                "registeredConnectionRemoved": True,
            },
            indent=2,
        )
    )

    def observe_completion(name):
        def complete():
            try:
                value = Observation(name + "-completion").ev(
                    "return await window[" + json.dumps(name) + "].completion;"
                )
                events.put(("complete", value))
            except BaseException as error:
                events.put(("error", type(error).__name__))

        thread = threading.Thread(target=complete)
        thread.start()
        threads.append(thread)

    # A real failed negotiation must terminate its unusable owned transport,
    # without waiting for the held auth decision or a second cleanup timer.
    (E / "authorized_keys").write_text("")
    assert attempt("failedNegotiation")
    observe_completion("failedNegotiation")
    kind, gate = events.get()
    if kind != "gate":
        gate = None
        raise AssertionError(
            "Native attempt completed before required auth gate; no auth observation"
        )
    fd, poll = owned_client()
    assert not poll.poll(0) and o.ev("return window.failedNegotiation.pending;")
    kind, failure = events.get()
    assert kind == "complete", "Failed negotiation must complete explicitly"
    assert not failure["ok"] and failure["error"].startswith(
        "recovery handshake failed:"
    ), "Real protocol deadline must remain an explicit negotiation failure"
    assert poll.poll(0), "Failed negotiation must reap its transport before completing"
    os.close(fd)
    gate.sendall(b"R")
    gate.close()
    gate = None
    (E / "FAILED-NEGOTIATION.json").write_text(
        json.dumps(
            {
                "nativeFailure": failure,
                "authHeldUntilCompletion": True,
                "childExitedAtCompletion": True,
            },
            indent=2,
        )
    )

    assert attempt("rejection")
    observe_completion("rejection")
    kind, gate = events.get()
    if kind != "gate":
        gate = None
        raise AssertionError(
            "Native attempt completed before required auth gate; no auth observation"
        )
    fd, poll = owned_client()
    assert o.ev("return window.rejection.pending;") and not poll.poll(0)
    gate.sendall(b"R")
    gate.close()
    gate = None
    kind, rejection = events.get()
    assert kind == "complete", "Rejected authentication must complete explicitly"
    assert (
        not rejection["ok"] and "Permission denied (publickey)" in rejection["error"]
    ), "Real authentication errors must remain visible"
    assert poll.poll(0), "Rejected authentication must settle with its child exited"
    os.close(fd)
    (E / "AUTH-REJECTION.json").write_text(
        json.dumps(
            {"nativeFailure": rejection, "childExitedAtCompletion": True}, indent=2
        )
    )
    gs.shutdown(socket.SHUT_RDWR)
    gate_thread.join()
    gate_thread = None
    shutil.copytree(Path(ready["storeDir"]) / "tracing", E / "tracing-before-stop")
    c.call("tyde_dev_instance_stop", {"instance_id": started["instanceId"]}, "STOP")
    instance_stopped = True
    assert not Path(ready["storeDir"]).exists() and all(
        not Path(d).exists() for d in ready["applicationDirectories"]
    )
    print("CANCEL_OBSERVATION_COMPLETE_AND_INSTANCE_CLEANED", flush=True)

finally:
    try:
        if "started" in locals() and not instance_stopped:
            if "ready" in locals() and Path(ready["storeDir"]).exists():
                shutil.copytree(
                    Path(ready["storeDir"]) / "tracing", E / "tracing-on-failure"
                )
            c.call(
                "tyde_dev_instance_stop",
                {"instance_id": started["instanceId"]},
                "STOP-ON-FAILURE",
            )
    finally:
        try:
            if gate is not None:
                try:
                    gate.sendall(b"R")
                except BrokenPipeError:
                    pass
                gate.close()
            if gate_thread is not None:
                gs.shutdown(socket.SHUT_RDWR)
                gate_thread.join()
                while not events.empty():
                    kind, value = events.get_nowait()
                    if kind == "gate":
                        value.sendall(b"R")
                        value.close()
            if gs is not None:
                gs.close()
        finally:
            for child in reversed(children):
                if child.poll() is None:
                    child.terminate()
            for child in reversed(children):
                child.wait()
            for t in threads:
                t.join()
            adopted = 0
            while True:
                try:
                    os.waitpid(-1, 0)
                    adopted += 1
                except ChildProcessError:
                    break
            try:
                if native is not None:
                    shutil.rmtree(native)
            finally:
                for name in [
                    "hostkey",
                    "hostkey.pub",
                    "unauthorized",
                    "unauthorized.pub",
                    "auth.sock",
                ]:
                    (E / name).unlink(missing_ok=True)
            (E / "JOINED.json").write_text(
                json.dumps(
                    {
                        "exits": [p.returncode for p in children],
                        "threadsJoined": True,
                        "adoptedDescendantsJoined": adopted,
                    }
                )
            )
            print("OWNED_CHILDREN_JOINED", flush=True)
