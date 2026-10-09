// Fixture for meshscale-builder characterization tests. A fixed build ID keeps
// response bodies comparable across rebuilds.
module.exports = {
  generateBuildId: async () => 'meshscale-fixture',
};
