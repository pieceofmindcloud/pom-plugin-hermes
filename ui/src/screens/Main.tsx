import { useCallback, useEffect, useRef, useState } from "react";
import { hostVersion, loader, onLoaderRetry, rpc, ui, usePluginI18n } from "../host/runtime";

type Step = "source" | "uv" | "python" | "packages" | "dashboard" | "connect";
type Status = {
  status: "waiting" | "installing" | "starting" | "ready" | "error";
  detail: string;
  step?: Step;
  progress?: number;
  mode?: "install" | "update";
};
const URL = "/api/ui/plugins/hermes/proxy/";

/**
 * The Hermes dashboard, behind POM's own loader and installer (host version 7).
 * The screen draws no loading state of its own: it maps the supervisor's status
 * to `loader.*` and installs by itself the first time someone opens it.
 */
export function Main() {
  const { t: translate } = usePluginI18n();
  // `t` is a new function on every render; the status effect must not run on renders.
  const t = useRef(translate);
  t.current = translate;
  const [status, setStatus] = useState<Status | null>(null);
  // An install ran in this mount: the end is a success, not a plain opening.
  const installed = useRef(false);
  // The install was asked for once already in this mount.
  const asked = useRef(false);
  // The dashboard was on screen, so a later error means it stopped.
  const opened = useRef(false);
  // "Try again" was clicked: an `error` read before the retry took hold is stale.
  const retrying = useRef(false);

  const load = useCallback(() => {
    rpc<Status>("status").then(setStatus, (reason: unknown) =>
      setStatus({ status: "error", detail: reason instanceof Error ? reason.message : String(reason) }),
    );
  }, []);
  const install = useCallback(() => {
    asked.current = true;
    rpc<{ error?: string }>("install", {}).then(
      (reply) => (reply?.error ? setStatus({ status: "error", detail: reply.error }) : load()),
      (reason: unknown) => setStatus({ status: "error", detail: reason instanceof Error ? reason.message : String(reason) }),
    );
  }, [load]);

  // Covered from the first render, before the first status answers.
  useEffect(() => void loader.show(), []);
  const installing = status?.status === "installing";
  useEffect(() => {
    load();
    const timer = window.setInterval(load, installing ? 1000 : 2500);
    return () => window.clearInterval(timer);
  }, [load, installing]);
  useEffect(
    () =>
      onLoaderRetry(() => {
        retrying.current = true;
        install();
      }),
    [install],
  );

  useEffect(() => {
    if (!status) return;
    if (status.status !== "error") retrying.current = false;
    const say = t.current;
    const title = say(status.mode === "update" ? "loader.updating" : "loader.installing");
    switch (status.status) {
      case "waiting":
        if (!asked.current) install();
        void loader.show();
        break;
      case "installing":
        installed.current = true;
        void loader.progress({ title, progress: status.progress ?? 0, message: status.step ? say(`step.${status.step}`) : undefined });
        break;
      case "starting":
        void (installed.current
          ? loader.progress({ title, progress: 100, message: say("step.starting") })
          : loader.show({ message: say("step.starting") }));
        break;
      case "error":
        if (retrying.current) break;
        void loader.error({ title: opened.current ? say("loader.stopped") : undefined, message: status.detail });
        break;
      case "ready":
        // The iframe's load opens the screen.
        break;
    }
  }, [status, install]);

  const frameLoaded = () => {
    opened.current = true;
    void (installed.current ? loader.success() : loader.hide());
    installed.current = false;
  };

  if (hostVersion() < 7) {
    const { ErrorState } = ui();
    return <ErrorState title={translate("main.title")} error={translate("main.hostTooOld")} />;
  }
  return (
    <div className="pb-page">
      {status?.status === "ready" ? (
        <iframe className="pb-dashboard" src={URL} title="Hermes Agent" allow="clipboard-read; clipboard-write"
          onLoad={frameLoaded} onError={frameLoaded} />
      ) : null}
    </div>
  );
}
