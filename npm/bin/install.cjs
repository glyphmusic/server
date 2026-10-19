#!/usr/bin/env node
'use strict';
require('../lib/install.cjs').install().catch(error => { console.error(`Glyph installation failed: ${error.message}`); process.exitCode = 1; });
