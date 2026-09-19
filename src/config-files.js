import { chmodSync, existsSync, statSync } from "node:fs";
import { platform } from "node:os";
import { atomicWriteJson, readJson } from "./utils.js";

const SUPPORTED_APIS = new Set([
  "anthropic-messages",
  "google-generative-ai",
  "openai-completions",
  "openai-responses",
]);

function assertObject(value, label) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${label} must be a JSON object`);
  }
}

function validateModelsDocument(document) {
  assertObject(document, "models.json");
  const providers = document.providers ?? {};
  assertObject(providers, "models.json providers");
  const summary = [];
  for (const [providerId, provider] of Object.entries(providers)) {
    if (!providerId.trim()) throw new Error("models.json provider IDs must be non-empty");
    assertObject(provider, `provider ${providerId}`);
    if (provider.api !== undefined && !SUPPORTED_APIS.has(provider.api)) {
      throw new Error(`provider ${providerId} uses unsupported api: ${provider.api}`);
    }
    if (provider.models !== undefined && !Array.isArray(provider.models)) {
      throw new Error(`provider ${providerId} models must be an array`);
    }
    const models = (provider.models ?? []).map((model, index) => {
      assertObject(model, `provider ${providerId} model ${index}`);
      if (typeof model.id !== "string" || !model.id.trim()) {
        throw new Error(`provider ${providerId} model ${index} requires a non-empty id`);
      }
      if (model.api !== undefined && !SUPPORTED_APIS.has(model.api)) {
        throw new Error(`provider ${providerId} model ${model.id} uses unsupported api: ${model.api}`);
      }
      return model.id;
    });
    summary.push({
      id: providerId,
      api: provider.api ?? null,
      hasBaseUrl: typeof provider.baseUrl === "string" && provider.baseUrl.length > 0,
      hasCredentialReference: typeof provider.apiKey === "string" && provider.apiKey.length > 0,
      hasCustomHeaders: provider.headers && typeof provider.headers === "object" && Object.keys(provider.headers).length > 0,
      models,
    });
  }
  return summary;
}

function privateMode(path) {
  return (statSync(path).mode & 0o777).toString(8).padStart(3, "0");
}

export function ensurePiConfigFiles(paths) {
  if (!existsSync(paths.modelsPath)) atomicWriteJson(paths.modelsPath, { providers: {} });
  if (!existsSync(paths.authPath)) atomicWriteJson(paths.authPath, {});
  chmodSync(paths.modelsPath, 0o600);
  chmodSync(paths.authPath, 0o600);
  return inspectPiConfig(paths);
}

export function inspectPiConfig(paths) {
  if (!existsSync(paths.modelsPath)) throw new Error(`missing AI API configuration: ${paths.modelsPath}`);
  if (!existsSync(paths.authPath)) throw new Error(`missing Pi authentication state: ${paths.authPath}`);
  const providers = validateModelsDocument(readJson(paths.modelsPath));
  const auth = readJson(paths.authPath);
  assertObject(auth, "auth.json");
  const modelsMode = privateMode(paths.modelsPath);
  const authMode = privateMode(paths.authPath);
  if (platform() !== "win32" && (modelsMode !== "600" || authMode !== "600")) {
    throw new Error(`AI configuration files must use mode 600 (models=${modelsMode}, auth=${authMode}); run pihub config init to repair permissions`);
  }
  return {
    modelsPath: paths.modelsPath,
    authPath: paths.authPath,
    modelsMode,
    authMode,
    providers,
    authProviders: Object.keys(auth).sort(),
  };
}
