; Подсветка комментариев
(comment) @comment

(global_defaults) @keyword
(fbl_set) @keyword

((identifier) @function.method
    (#match? @function.method "^(set-header|add-header)$"))

(identifier) @attribute

(section_name) @string

((word) @keyword
    (#match? @keyword "^(enable|disable|ssl|crt)$"))

((word) @constant
    (#match? @constant "^(if|acl|use_backend)$"))

((word) @constant
    (#match? @constant "^(http|https|ssl_fc|X-Forwarded-Port|X-Forwarded-Proto|forwardfor)$"))

((word) @number
    (#match? @number "^(\\d+)(.)?$"))

((word) @function
    (#match? @function "^(\.+)\/(\[^\/\]+)$"))

; ((word) @variable
;     (#match? @variable "^(\\d+)|(\\*:(\\d+))$"))

((word) @function
    (#match? @function "^(?:[0-9]{1,3}\\.){3}[0-9]{1,3}$"))

((word) @function
    (#match? @function "^(?:[0-9]{1,3}\\.){3}[0-9]{1,3}:\\d{1,5}$"))

((word) @function
    (#match? @function "^\\*:\\d{1,5}$"))

((word) @function
    (#match? @function "^(set-header|add-header|hdr)$"))
