package metadata

components: transforms: remove_tags: {
	title:             "Remove Tags"
	short_description: "Accepts metric events and allows you to remove one or more metric tags."
	long_description:  "Accepts metric events and allows you to remove one or more metric tags."

	classes: {
		commonly_used: false
		egress_method: "stream"
		function:      "schema"
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
		warnings: []
		notices: []
	}

	configuration: {
		tags: {
			description: "The tag names to drop."
			required:    true
			warnings: []
			type: array: items: type: string: examples: ["tag1", "tag2"]
		}
	}

	input: {
		logs: false
		metrics: {
			counter:      true
			distribution: true
			gauge:        true
			histogram:    true
			set:          true
			summary:      true
		}
	}
}
