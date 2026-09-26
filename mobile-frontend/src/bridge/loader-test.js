let previous;
let selected;
let target;
let calls = [];
let commits = [];
export function install() {
  previous = window.__tydeLoader;
  selected = localStorage.getItem("tyde.selected-host.v1");
  target = localStorage.getItem("tyde.loader.host-target.v1");
  localStorage.removeItem("tyde.loader.host-target.v1");
  calls = []; commits = [];
  window.__tydeLoader = {
    bootVersion: () => "0.9.4-beta.11",
    cancelHostSwitch: () => {},
    selectHost: host => {
      const target = JSON.parse(localStorage.getItem("tyde.loader.host-target.v1") || "null");
      if (target && target.host !== host) localStorage.removeItem("tyde.loader.host-target.v1");
      if (host === null) localStorage.removeItem("tyde.selected-host.v1");
      else localStorage.setItem("tyde.selected-host.v1", host);
      return {status: "matching"};
    },
    recordHostRelease: (version, protocolVersion, host) => {
      localStorage.setItem("tyde.loader.host-target.v1", JSON.stringify({host,version,protocolVersion}));
      return {status: "matching"};
    },
    confirmHostRelease: () => ({status: "matching"}),
    prepareHostSwitch: (version, protocol, host) => new Promise(resolve => {
      calls.push({version, protocol, host, resolve});
    }),
    commitHostSwitch: (ticket, host) => {
      commits.push({ticket, host});
      return {status: "reloading"};
    },
  };
}
export function finish(index, reason) {
  const call = calls[index];
  if (!call) throw new Error("missing preparation");
  call.resolve(reason
    ? {status: "unavailable", reason}
    : {status: "ready", ticket: index + 1});
}
export function preparations() { return calls.length; }
export function reloads() { return commits.length; }
export function ownsTarget(host, version) {
  const target = JSON.parse(localStorage.getItem("tyde.loader.host-target.v1") || "null");
  return target?.host === host && target?.version === version;
}
export function noTarget() { return localStorage.getItem("tyde.loader.host-target.v1") === null; }
export function restore() {
  if (target === null) localStorage.removeItem("tyde.loader.host-target.v1");
  else localStorage.setItem("tyde.loader.host-target.v1", target);
  window.__tydeLoader = previous;
  if (selected === null) localStorage.removeItem("tyde.selected-host.v1");
  else localStorage.setItem("tyde.selected-host.v1", selected);
}
