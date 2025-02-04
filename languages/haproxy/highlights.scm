(comment) @comment

((identifier) @keyword
  (#match? @keyword "^(global|daemon|log|maxconn|retries|timeout|listen|bind|mode|stats|acl|use_backend)$"))
