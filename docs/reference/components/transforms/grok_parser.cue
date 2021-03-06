package metadata

components: transforms: grok_parser: {
	title:             "Grok Parser"
	short_description: "Accepts log events and allows you to parse a log field value with [Grok][urls.grok]."
	long_description:  "Accepts log events and allows you to parse a log field value with [Grok][urls.grok]."

	classes: {
		commonly_used: false
		egress_method: "stream"
		function:      "parse"
	}

	features: {}

	statuses: {
		development: "stable"
	}

	support: {
		platforms: {
			triples: {
				"aarch64-unknown-linux-gnu":  true
				"aarch64-unknown-linux-musl": true
				"x86_64-apple-darwin":        true
				"x86_64-pc-windows-msv":      true
				"x86_64-unknown-linux-gnu":   true
				"x86_64-unknown-linux-musl":  true
			}
		}

		requirements: []
		warnings: [
			#"""
				Grok is approximately 50% slower than the [`regex_parser` transform][docs.transforms.regex_parser].
				While this is still plenty fast for most use cases we recommend using the
				[`regex_parser` transform][docs.transforms.regex_parser] if you are experiencing
				performance issues.
				"""#,
		]
		notices: [
			#"""
				Vector uses the Rust [`grok` library][urls.rust_grok_library]. All patterns
				[listed here][urls.grok_patterns] are supported. It is recommended to use
				maintained patterns when possible since they will be improved over time by
				the community.
				"""#,
		]
	}

	configuration: {
		drop_field: {
			common:      true
			description: "If `true` will drop the specified `field` after parsing."
			required:    false
			warnings: []
			type: bool: default: true
		}
		field: {
			common:      true
			description: "The log field to execute the `pattern` against. Must be a `string` value."
			required:    false
			warnings: []
			type: string: {
				default: "message"
				examples: ["message", "parent.child", "array[0]"]
			}
		}
		pattern: {
			description: "The [Grok pattern][urls.grok_patterns]"
			required:    true
			warnings: []
			type: string: {
				examples: ["%{TIMESTAMP_ISO8601:timestamp} %{LOGLEVEL:level} %{GREEDYDATA:message}"]
			}
		}
		types: {
			common:      true
			description: "Key/value pairs representing mapped log field names and types. This is used to coerce log fields into their proper types."
			required:    false
			warnings: []
			type: object: {
				examples: [{"status": "int"}, {"duration": "float"}, {"success": "bool"}, {"timestamp": "timestamp|%F"}, {"timestamp": "timestamp|%a %b %e %T %Y"}, {"parent": {"child": "int"}}]
				options: {}
			}
		}
	}

	input: {
		logs:    true
		metrics: false
	}

	how_it_works: {
		testing: {
			title: "Testing"
			body: #"""
				We recommend the [Grok debugger][urls.grok_debugger] for Grok testing.
				"""#
		}
	}
}
