; Comments
(comment) @comment

; ---------------------------------------------------------------------------
; Section headers — anchored-token pattern: keyword + section name
; ---------------------------------------------------------------------------
(global_section    "global"    @keyword)
(defaults_section  "defaults"  @keyword)
(defaults_section  (section_name) @variable)
(frontend_section  "frontend"  @keyword)
(frontend_section  (section_name) @variable)
(backend_section   "backend"   @keyword)
(backend_section   (section_name) @variable)
(listen_section    "listen"    @keyword)
(listen_section    (section_name) @variable)
(resolvers_section "resolvers" @keyword)
(resolvers_section (section_name) @variable)
(userlist_section  "userlist"  @keyword)
(userlist_section  (section_name) @variable)
(peers_section     "peers"     @keyword)
(peers_section     (section_name) @variable)
(mailers_section   "mailers"   @keyword)
(mailers_section   (section_name) @variable)
(cache_section     "cache"     @keyword)
(cache_section     (section_name) @variable)
(program_section   "program"   @keyword)
(program_section   (section_name) @variable)
(ring_section      "ring"      @keyword)
(ring_section      (section_name) @variable)

; ---------------------------------------------------------------------------
; Control flow: conditionals, operators, and phase tokens
; ---------------------------------------------------------------------------
"if"     @keyword.control
"unless" @keyword.control
(tcp_type) @keyword.control

; ---------------------------------------------------------------------------
; Function.builtin — declarations (MUST precede any broad directive_name rule)
; ---------------------------------------------------------------------------
(bind_directive)   @function.builtin
(server_directive) @function.builtin
(acl_directive)    @function.builtin
(peer_directive)   @function.builtin
(generic_directive (directive_name) @function.builtin
  (#match? @function.builtin "^stick-table$"))

; ---------------------------------------------------------------------------
; Function — routing/action directives
; ---------------------------------------------------------------------------
(use_backend_directive)     @function
(default_backend_directive) @function
(use_server_directive)      @function
(generic_directive (directive_name) @function
  (#match? @function "^(http-request|http-response|tcp-request|redirect|use-service|http-after-response)$"))

; ---------------------------------------------------------------------------
; Attribute — settings directives
; ---------------------------------------------------------------------------
(timeout_directive)       @attribute
(maxconn_directive)       @attribute
(nbproc_directive)        @attribute
(nbthread_directive)      @attribute
(balance_directive)       @attribute
(stats_directive)         @attribute
(daemon_directive)        @attribute
(log_directive)           @attribute
(ca_base_directive)       @attribute
(crt_base_directive)      @attribute
(chroot_directive)        @attribute
(pidfile_directive)       @attribute
(tune_directive)          @attribute
(ssl_default_directive)   @attribute
(cpu_map_directive)       @attribute
(mode_directive)          @attribute
(option_directive)        @attribute
(no_option_directive)     @attribute
(description_directive)   @attribute
(node_directive)          @attribute
(uid_directive)           @attribute
(gid_directive)           @attribute

; ---------------------------------------------------------------------------
; Type — enumerated values
; ---------------------------------------------------------------------------
(mode_type)         @type
(balance_algorithm) @type
(log_level)         @type
(timeout_type)      @type

; ---------------------------------------------------------------------------
; Property — option names scoped to option_directive
; ---------------------------------------------------------------------------
(option_directive (option_name) @property)

; ---------------------------------------------------------------------------
; Variable — identifiers and references
; ---------------------------------------------------------------------------
(acl_name)    @variable
(backend_ref) @variable
(server_name) @variable

; ---------------------------------------------------------------------------
; String.special — addresses, paths, log targets
; ---------------------------------------------------------------------------
(bind_address)   @string.special
(server_address) @string.special
(log_target)     @string.special
(path)           @string.special
(address)        @string.special

; ---------------------------------------------------------------------------
; String — literal strings and ACL criteria
; ---------------------------------------------------------------------------
(string)        @string
(acl_criterion) @string

; ---------------------------------------------------------------------------
; Number — numeric literals, sizes, times, status codes
; ---------------------------------------------------------------------------
(number)      @number
(size)        @number
(time_value)  @number
(http_status) @number
