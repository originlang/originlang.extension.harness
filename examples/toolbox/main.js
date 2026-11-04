// A tool provider. It knows nothing about the harness: `contributes.tools` is
// the whole contract, and the harness discovers it the moment the package is
// installed. That is the point of the split — a plugin that adds a capability
// never has to be told about the plugin that consumes it.
Origin.define({
  id: "toolbox",
  version: "0.1.0",
  capabilities: ["storage.read", "storage.write", "log.write"],

  activate() {
    Origin.host.log("toolbox ready", "debug");
  },

  methods: {
    note_save(params) {
      const key = String(params?.key ?? "");
      if (!key) throw new Error("note_save requires a key");
      Origin.host.storage.set(`note:${key}`, String(params.value ?? ""));
      return { saved: key };
    },

    note_read(params) {
      const key = String(params?.key ?? "");
      const value = Origin.host.storage.get(`note:${key}`);
      return { key, value: value ?? null };
    },

    add(params) {
      const a = Number(params?.a);
      const b = Number(params?.b);
      if (!Number.isFinite(a) || !Number.isFinite(b)) {
        throw new Error("add requires two finite numbers");
      }
      return { sum: a + b };
    },
  },
});
