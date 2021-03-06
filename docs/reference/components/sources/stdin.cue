package metadata

components: sources: stdin: {
	title:             "STDIN"
	short_description: "Ingests data through [standard input (STDIN)][urls.stdin] and outputs log events."
	long_description:  "Ingests data through [standard input (STDIN)][urls.stdin] and outputs log events."

	classes: {
		commonly_used: false
		deployment_roles: ["sidecar"]
		egress_method: "stream"
		function:      "receive"
	}

	features: {
		checkpoint: enabled: false
		multiline: enabled:  false
		tls: enabled:        false
	}

	statuses: {
		delivery:    "at_least_once"
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
		warnings: []
		notices: []
	}

	configuration: {
		host_key: {
			common:      false
			description: "The key name added to each event representing the current host. This can also be globally set via the [global `host_key` option][docs.reference.global-options#host_key]."
			required:    false
			warnings: []
			type: string: default: "host"
		}
		max_length: {
			common:      false
			description: "The maximum bytes size of a message before rest of it will be discarded."
			required:    false
			warnings: []
			type: uint: {
				default: 102400
				unit:    "bytes"
			}
		}
	}

	output: logs: line: {
		description: "An individual event from STDIN."
		fields: {
			host:      fields._local_host
			message:   fields._raw_line
			timestamp: fields._current_timestamp
		}
	}

	examples: [
		{
			_line: #"""
				2019-02-13T19:48:34+00:00 [info] Started GET "/" for 127.0.0.1
				"""#
			title: "STDIN line"
			configuration: {}
			input: """
				```text
				\( _line )
				```
				"""
			output: log: {
				timestamp: _values.current_timestamp
				message:   _line
				host:      _values.local_host
			}
		}]
}
