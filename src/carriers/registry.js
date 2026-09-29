const CARRIER_ID = /^[a-z][a-z0-9_-]*$/;

function assertAdapter(adapter) {
  if (!adapter || !CARRIER_ID.test(adapter.id || "")) {
    throw new Error("carrier adapter must have a lowercase id");
  }
  if (typeof adapter.displayName !== "string" || !adapter.displayName) {
    throw new Error(`carrier ${adapter.id} must have a displayName`);
  }
  if (typeof adapter.diagnose !== "function" || typeof adapter.createRuntime !== "function") {
    throw new Error(`carrier ${adapter.id} must implement diagnose and createRuntime`);
  }
  if (!adapter.capabilities || typeof adapter.capabilities !== "object"
    || typeof adapter.capabilities.skillLoadingSelection !== "boolean"
    || !Array.isArray(adapter.capabilities.messages)) {
    throw new Error(`carrier ${adapter.id} must declare messaging and Skill loading selection capabilities`);
  }
}

export class CarrierRegistry {
  constructor(adapters = []) {
    this.adapters = new Map();
    for (const adapter of adapters) this.register(adapter);
  }

  register(adapter) {
    assertAdapter(adapter);
    if (this.adapters.has(adapter.id)) throw new Error(`carrier already registered: ${adapter.id}`);
    this.adapters.set(adapter.id, adapter);
    return this;
  }

  get(id) {
    const adapter = this.adapters.get(id);
    if (!adapter) throw new Error(`unknown carrier: ${id}`);
    return adapter;
  }

  list() {
    return [...this.adapters.values()].map(({ id, displayName, capabilities }) => ({
      id,
      displayName,
      capabilities: { ...capabilities },
    }));
  }

  async diagnose(id, context = {}) {
    const adapter = this.get(id);
    try {
      const result = await adapter.diagnose(context);
      return {
        id: adapter.id,
        displayName: adapter.displayName,
        capabilities: { ...adapter.capabilities },
        available: Boolean(result?.available),
        reason: result?.reason || null,
      };
    } catch (error) {
      return {
        id: adapter.id,
        displayName: adapter.displayName,
        capabilities: { ...adapter.capabilities },
        available: false,
        reason: error.message,
      };
    }
  }

  createRuntime(id, context = {}) {
    const adapter = this.get(id);
    if (context.spec?.authorization?.mode === "only" && !adapter.capabilities.skillLoadingSelection) {
      throw new Error(`carrier ${id} cannot select the Skill loading set`);
    }
    const runtime = adapter.createRuntime(context);
    if (!runtime || typeof runtime.on !== "function"
      || typeof runtime.start !== "function"
      || typeof runtime.request !== "function"
      || typeof runtime.stop !== "function") {
      throw new Error(`carrier ${id} returned an invalid runtime`);
    }
    return runtime;
  }
}
