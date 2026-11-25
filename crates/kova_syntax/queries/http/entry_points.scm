; Kova IDE Tree-sitter entry-point queries for HTTP request files (.http)
; Matches REST requests with a method and target URL.

(request
  method: (method) @http.method
  url: (target_url) @http.url) @http.request
