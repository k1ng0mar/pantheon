/* Settings → Remote access: sign in to reach your other computers, or pair them directly
 * on this network with a code. Nothing reaches the network until you ask it to. */
import './remote-settings.css';
import { escape, icon } from './dom';
import { infoMark } from './tooltip';
import type { RemoteDirect, RemoteDevice, RemoteHost, RemoteSignIn, RemoteStatus } from './remote-types';

const TRUST_TIP = 'Both computers must trust each other before one can drive the other. Compare the fingerprint shown on the other computer with the one here before trusting it. If its key changes, you need to trust it again.';
const ALLOW_TIP = 'Remote prompts follow this computer’s approval settings; they get no more power than prompts typed here.';
const PAIR_TIP = 'A direct connection is one computer to another: on this network, over Tailscale, or through a self-hosted endpoint. No relay, no sign-in. The pairing code still proves the key exchange, so a stranger on the wire cannot impersonate either side.';

export function mountRemoteSettings(root: HTMLElement, host: RemoteHost): () => void {
  let status: RemoteStatus | undefined, devices: RemoteDevice[] = [], error = '', code: RemoteSignIn | undefined, expires = 0, signingIn = false, disposed = false, attempt = 0;
  let pairCode = '', pairSeconds = 0;
  const fail = (e: unknown) => { error = e instanceof Error ? e.message : String(e); render(); };

  function deviceRow(device: RemoteDevice): string {
    return `<div class="setting-row remote-device" data-device="${escape(device.id)}"><div><strong><span class="remote-dot" data-online="${device.online}" role="img" aria-label="${device.online ? 'Online' : 'Offline'}"></span>${escape(device.name)}${device.current ? ' <span class="remote-badge">This computer</span>' : ''}</strong><small>${escape(device.platform)} · ${device.host ? 'remote control on' : 'remote control off'} · key <code>${escape(device.fingerprint)}</code></small></div>${device.current ? '' : `<label class="remote-trust"><span>Trusted ${infoMark(TRUST_TIP, 'About trusting a computer')}</span><input type="checkbox" class="settings-switch" data-trust="${escape(device.id)}" aria-label="Trusted: ${escape(device.name)}" ${device.trusted ? 'checked' : ''}></label>`}${device.current ? '' : `<button type="button" class="secondary-button" data-revoke="${escape(device.id)}" aria-label="Revoke ${escape(device.name)}">Revoke</button>`}</div>`;
  }

  function relayBody(): string {
    if (!status!.configured) return '<p class="remote-note" role="status">This build has no relay, so GitHub sign-in is off. The direct connection below works without one.</p>';
    if (!status!.signedIn) {
      const flow = code ? `<div class="remote-code"><p>Enter this code at <strong>${escape(code.verificationUri)}</strong>:</p><div class="remote-code-row"><output class="remote-user-code" aria-label="Device code">${escape(code.userCode)}</output><button type="button" class="secondary-button" data-copy>${icon('copy')}Copy code</button></div><p class="remote-note" role="status">Waiting for you to approve in the browser… The code expires at ${new Date(expires).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })}.</p><button type="button" class="secondary-button" data-cancel>Cancel</button></div>` : '';
      return `<div class="setting-row"><div><strong>Pantheon Connect</strong><small>Sign in with GitHub to reach your other computers. Task content is end-to-end encrypted; the relay sees only ciphertext.</small></div><button type="button" class="secondary-button" data-sign-in ${signingIn ? 'disabled' : ''}>${icon('github')}Sign in with GitHub</button></div>${flow}`;
    }
    return `<div class="setting-row"><div><strong>${escape(status!.account?.login ?? 'Signed in')}</strong><small>Signed in with GitHub · ${status!.connected ? 'connected' : 'connecting…'}</small></div><button type="button" class="secondary-button" data-sign-out>Sign out</button></div>
      <div class="setting-row"><div><strong>Allow remote control of this computer ${infoMark(ALLOW_TIP, 'About remote control')}</strong><small>Your other signed-in computers can see and drive tasks here.</small></div><input type="checkbox" class="settings-switch" id="remote-host" aria-label="Allow remote control of this computer" ${status!.hostEnabled ? 'checked' : ''}></div>
      <h3 class="settings-heading">Devices</h3><div class="settings-group remote-devices">${devices.length ? devices.map(deviceRow).join('') : '<p class="remote-note">No devices yet.</p>'}</div>`;
  }

  function directBody(): string {
    const direct: RemoteDirect = status!.direct;
    const online = (id: string) => direct.connected.includes(id);
    const listening = direct.listening
      ? `<div class="setting-row"><div><strong>Listening on port ${direct.port}</strong><small>Another computer pairs here with the code below, on this network, over Tailscale, or through your own endpoint.</small></div><button type="button" class="secondary-button" data-direct-stop>Stop</button></div>
         <div class="remote-code"><div class="remote-code-row"><output class="remote-user-code" aria-label="Pairing code">${escape(pairCode)}</output><button type="button" class="secondary-button" data-direct-copy>${icon('copy')}Copy code</button></div><p class="remote-note" role="status">Type this code on the computer joining this one. It expires in ${Math.max(1, Math.ceil(pairSeconds / 60))} min.</p></div>`
      : `<div class="setting-row"><div><strong>Open a direct connection ${infoMark(PAIR_TIP, 'About direct connections')}</strong><small>Let another computer pair with this one. No relay, no sign-in.</small></div><button type="button" class="secondary-button" data-direct-start>Open</button></div>`;
    const join = `<div class="setting-row"><div><strong>Join a computer</strong><small>Enter its address and the code it shows. host:port on this network, a Tailscale address, or wss:// to a self-hosted endpoint.</small></div><div class="remote-join"><input type="text" data-direct-address placeholder="192.168.1.5:7447" aria-label="Computer address"><input type="text" data-direct-code placeholder="Code" aria-label="Code from that computer"><button type="button" class="secondary-button" data-direct-join>Pair</button></div></div>`;
    const paired = direct.hosts.length
      ? `<h3 class="settings-heading">Paired computers</h3><div class="settings-group remote-devices">${direct.hosts.map(item => `<div class="setting-row remote-device" data-host="${escape(item.deviceId)}"><div><strong><span class="remote-dot" data-online="${online(item.deviceId)}" role="img" aria-label="${online(item.deviceId) ? 'Online' : 'Offline'}"></span>${escape(item.name || item.address)}</strong><small>${escape(item.address)}</small></div><button type="button" class="secondary-button" data-direct-connect="${escape(item.address)}">Connect</button><button type="button" class="secondary-button" data-direct-forget="${escape(item.deviceId)}" aria-label="Forget ${escape(item.name || item.address)}">Forget</button></div>`).join('')}</div>`
      : '';
    return `<h3 class="settings-heading">Direct connection</h3>${listening}${join}${paired}`;
  }

  function body(): string {
    if (!status) return '<p class="dialog-loading" role="status">Loading…</p>';
    return relayBody() + directBody();
  }
  function render(): void {
    if (disposed) return;
    root.innerHTML = `<div class="settings-group remote-settings">${body()}</div>${error ? `<p class="remote-error" role="alert">${escape(error)}</p>` : ''}`;
  }
  const apply = (next: RemoteStatus) => { status = next; if (next.signedIn) { code = undefined; signingIn = false; } };
  async function load(): Promise<void> {
    try {
      apply(await host.call<RemoteStatus>('remote_status'));
      devices = status!.signedIn ? await host.call<RemoteDevice[]>('remote_devices') : [];
    } catch (e) { error = e instanceof Error ? e.message : String(e); }
    render();
  }
  async function signIn(): Promise<void> {
    if (signingIn) return;
    error = ''; signingIn = true;
    const mine = ++attempt;
    try {
      code = await host.call<RemoteSignIn>('remote_sign_in_start'); expires = Date.now() + code.expiresIn * 1000; render();
      void host.openUrl(code.verificationUri).catch(() => undefined);
      apply(await host.call<RemoteStatus>('remote_sign_in_finish'));
      devices = await host.call<RemoteDevice[]>('remote_devices');
    } catch (e) {
      if (mine !== attempt) return;
      code = undefined; signingIn = false; error = e instanceof Error ? e.message : String(e);
    }
    render();
  }
  function cancelSignIn(): void {
    attempt++; code = undefined; signingIn = false; error = ''; render();
    void host.call('remote_sign_in_cancel').catch(fail);
  }
  function field(attribute: string): string {
    return (root.querySelector(`[${attribute}]`) as HTMLInputElement | null)?.value.trim() ?? '';
  }
  async function openDirect(): Promise<void> {
    error = '';
    try {
      const next = await host.call<{ port: number; direct: RemoteDirect }>('remote_direct_start');
      status!.direct = next.direct;
      const code = await host.call<{ code: string; expiresIn: number }>('remote_direct_code');
      pairCode = code.code; pairSeconds = code.expiresIn;
    } catch (e) { fail(e); }
    render();
  }
  async function joinDirect(): Promise<void> {
    const address = field('data-direct-address'), typed = field('data-direct-code');
    if (!address) { error = 'Enter the computer’s address as host:port.'; render(); return; }
    const paired = status!.direct.hosts.some(host => host.address === address);
    if (!typed && !paired) { error = 'Enter the pairing code that computer shows.'; render(); return; }
    error = '';
    try {
      const next = await host.call<{ direct: RemoteDirect }>('remote_direct_connect', { address, code: typed });
      status!.direct = next.direct;
    } catch (e) { fail(e); }
    render();
  }
  root.addEventListener('click', event => {
    const target = (event.target as HTMLElement).closest<HTMLElement>('button');
    if (!target) return;
    if (target.hasAttribute('data-sign-in')) void signIn();
    else if (target.hasAttribute('data-cancel')) cancelSignIn();
    else if (target.hasAttribute('data-copy') && code) void navigator.clipboard.writeText(code.userCode).then(() => host.notify('Code copied'), () => host.notify('Couldn’t copy; select the code instead'));
    else if (target.hasAttribute('data-sign-out')) void host.call<RemoteStatus>('remote_sign_out').then(next => { apply(next); devices = []; error = ''; render(); }, fail);
    else if (target.dataset.revoke) void host.call('remote_revoke_device', { deviceId: target.dataset.revoke }).then(() => { devices = devices.filter(d => d.id !== target.dataset.revoke); render(); }, fail);
    else if (target.hasAttribute('data-direct-start')) void openDirect();
    else if (target.hasAttribute('data-direct-stop')) void host.call<RemoteStatus>('remote_direct_stop').then(next => { apply(next); pairCode = ''; pairSeconds = 0; render(); }, fail);
    else if (target.hasAttribute('data-direct-copy')) void navigator.clipboard.writeText(pairCode).then(() => host.notify('Code copied'), () => host.notify('Couldn’t copy; select the code instead'));
    else if (target.hasAttribute('data-direct-join')) void joinDirect();
    else if (target.dataset.directConnect) void host.call<{ direct: RemoteDirect }>('remote_direct_connect', { address: target.dataset.directConnect, code: '' }).then(next => { status!.direct = next.direct; error = ''; render(); }, fail);
    else if (target.dataset.directForget) void host.call<{ direct: RemoteDirect }>('remote_direct_forget', { deviceId: target.dataset.directForget }).then(next => { status!.direct = next.direct; render(); }, fail);
  });
  root.addEventListener('change', event => {
    const input = event.target as HTMLInputElement;
    if (input.dataset.trust) void host.call<RemoteDevice[]>('remote_set_trusted', { deviceId: input.dataset.trust, trusted: input.checked }).then(next => { devices = next; render(); }, e => { input.checked = !input.checked; fail(e); });
    else if (input.id === 'remote-host') void host.call<RemoteStatus>('remote_set_host_enabled', { enabled: input.checked }).then(next => { apply(next); render(); }, e => { input.checked = !input.checked; fail(e); });
  });
  const unlisten: Promise<() => void>[] = [
    host.listen('remote://status', next => { if (!signingIn || next.signedIn) { apply(next); render(); } }),
    host.listen('remote://presence', next => { if (Array.isArray(next)) { devices = next; render(); } }),
  ];
  void load();
  return () => { disposed = true; for (const u of unlisten) void u.then(fn => fn(), () => undefined); };
}
