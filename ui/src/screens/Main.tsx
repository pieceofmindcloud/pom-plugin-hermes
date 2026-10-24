import { useCallback, useEffect, useState } from "react";
import { rpc, ui, usePluginI18n } from "../host/runtime";
import { useScreenReady } from "../host/ready";

type Status = { status: "waiting" | "installing" | "starting" | "ready" | "error"; detail: string; error?: string };
const URL = "/api/ui/plugins/hermes/proxy/";

export function Main() {
  const { t } = usePluginI18n();
  const { PageHeader, Panel, PanelHeader, Button, Badge, ErrorState } = ui();
  const [status, setStatus] = useState<Status | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [model, setModel] = useState("");
  const [showSetup, setShowSetup] = useState(false);
  const [frameLoaded, setFrameLoaded] = useState(false);
  const load = useCallback(() => {
    rpc<Status>("status").then((value) => { setStatus(value); setError(null); }, setError);
  }, []);
  useEffect(() => {
    load();
    const timer = window.setInterval(load, 2500);
    return () => window.clearInterval(timer);
  }, [load]);
  useScreenReady((status !== null && (status.status !== "ready" || showSetup || frameLoaded)) || error !== null);

  const install = async () => {
    try {
      const reply = await rpc<Status>("install", { model: model.trim() || null });
      if (reply.error) setError(reply.error);
      else { setError(null); load(); }
    } catch (reason) { setError(reason); }
  };

  if (!status && error) return <ErrorState title={t("main.title")} error={error} onRetry={load} />;
  return <div className="pb-page">
    <PageHeader title={t("main.title")} subtitle={t("main.subtitle")}
      actions={status?.status === "ready" ? <Button variant="outline" size="sm" onClick={() => setShowSetup(!showSetup)}>{showSetup ? t("main.open") : t("main.setup")}</Button> : undefined} />
    {status?.status === "ready" && !showSetup ? (
      <iframe className="pb-dashboard" src={URL} title="Hermes Agent" allow="clipboard-read; clipboard-write"
        onLoad={() => setFrameLoaded(true)} onError={() => setFrameLoaded(true)} />
    ) : <Panel>
      <PanelHeader title={t("main.setup")} />
      <div className="pb-setup">
        <Badge tone={status?.status === "ready" ? "aqua" : status?.status === "error" ? "rose" : "amber"}>{status?.status ?? t("main.loading")}</Badge>
        <p className="pb-muted">{status?.detail ?? t("main.loading")}</p>
        <p>{t("main.explain")}</p>
        <label htmlFor="hermes-model">{t("main.model")}</label>
        <input id="hermes-model" className="pb-input" value={model} onChange={(e) => setModel(e.target.value)} placeholder={t("main.modelPlaceholder")} disabled={status?.status === "installing" || status?.status === "starting" || status?.status === "ready"} />
        <p className="pb-muted">{t("main.optional")}</p>
        {error && <p role="alert" className="pb-error">{String(error)}</p>}
        {status?.status !== "ready" && <Button onClick={() => void install()} disabled={status?.status === "installing" || status?.status === "starting"}>{status?.status === "error" ? t("main.retry") : t("main.install")}</Button>}
      </div>
    </Panel>}
  </div>;
}
