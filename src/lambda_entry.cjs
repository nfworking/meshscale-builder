'use strict';

// AWS Lambda handler for a MeshScale artifact (handler string: lambda-entry.handler).
// The require paths are the generated runtime/ file names.
const { createHandler } = require('./lambda-adapter.cjs');
const { createRuntime } = require('./runtime.cjs');

exports.handler = createHandler({ start: () => createRuntime({ root: __dirname }) });
