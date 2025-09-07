; Comments
(comment) @comment

; Section names
(section_name) @string

; Directive types
(daemon_directive) @keyword
(log_directive) @keyword
(maxconn_directive) @keyword
(retries_directive) @keyword
(timeout_directive) @keyword
(bind_directive) @keyword
(mode_directive) @keyword
(balance_directive) @keyword
(server_directive) @keyword
(use_backend_directive) @keyword
(acl_directive) @keyword
(generic_directive) @function

; Directive names (for generic directives)
(directive_name) @function

; Values
(number) @number
(time_value) @number

; Addresses and targets
(bind_address) @string.special
(server_address) @string.special
(log_target) @string.special

; Types and levels
(log_level) @constant
(mode_type) @constant
(timeout_type) @attribute
(balance_algorithm) @constant

; Names and references
(server_name) @variable
(backend_ref) @variable
(acl_name) @variable
(acl_criterion) @string
