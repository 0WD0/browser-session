;;; browser-session-test.el --- Tests for browser-session -*- lexical-binding: t; -*-

;; Copyright (C) 2026
;; SPDX-License-Identifier: MIT

;;; Code:

(require 'ert)
(require 'json)
(require 'browser-session)

(ert-deftest browser-session-default-helper-follows-symlinked-source ()
  "Resolve a straight-style build's helper from its source checkout."
  (let* ((root (make-temp-file "browser-session-helper-" t))
         (build-directory (expand-file-name "build" root))
         (source-directory (expand-file-name "source" root))
         (source-el (expand-file-name "browser-session.el" source-directory))
         (build-el (expand-file-name "browser-session.el" build-directory))
         (helper (expand-file-name
                  (concat "target/debug/"
                          (browser-session--helper-executable-name))
                  source-directory)))
    (unwind-protect
        (progn
          (make-directory build-directory)
          (make-directory (file-name-directory helper) t)
          (with-temp-file source-el
            (insert ";; source marker\n"))
          (with-temp-file helper
            (insert "#!/bin/sh\nexit 0\n"))
          (set-file-modes helper #o700)
          (condition-case error
              (make-symbolic-link source-el build-el)
            (file-error
             (ert-skip (format "Cannot create a symbolic link: %s"
                               (error-message-string error)))))
          (should (equal (browser-session--default-helper-command build-directory)
                         helper)))
      (delete-directory root t))))

(ert-deftest browser-session-capture-uses-an-isolated-profile-root ()
  "Capture should use the configured automatic profile root by default."
  (let ((browser-session-profile-directory nil)
        (browser-session-profile-root "/tmp/browser-session-root")
        arguments)
    (cl-letf (((symbol-function 'browser-session--run-helper)
               (lambda (value _callback _errorback &optional _input)
                 (setq arguments value)
                 'capture-process)))
      (browser-session-capture
       :url "https://x.com"
       :cookies '("auth_token")
       :output-file "/tmp/browser-session-capture.json")
      (should (member "--profile-root" arguments))
      (should (member "/tmp/browser-session-root" arguments)))))

(ert-deftest browser-session-explicit-profile-directory-overrides-root ()
  "An explicit profile directory should suppress the automatic profile root."
  (let ((browser-session-profile-directory nil)
        (browser-session-profile-root "/tmp/browser-session-root")
        arguments)
    (cl-letf (((symbol-function 'browser-session--run-helper)
               (lambda (value _callback _errorback &optional _input)
                 (setq arguments value)
                 'capture-process)))
      (browser-session-capture
       :url "https://x.com"
       :cookies '("auth_token")
       :output-file "/tmp/browser-session-capture.json"
       :profile-directory "/tmp/browser-session-profile")
      (should (member "--profile-dir" arguments))
      (should (member "/tmp/browser-session-profile" arguments))
      (should-not (member "--profile-root" arguments)))))

(ert-deftest browser-session-call-profile-root-overrides-global-directory ()
  "A caller's profile root should win over a global profile directory."
  (let ((browser-session-profile-directory "/tmp/browser-session-global-profile")
        (browser-session-profile-root "/tmp/browser-session-global-root")
        arguments)
    (cl-letf (((symbol-function 'browser-session--run-helper)
               (lambda (value _callback _errorback &optional _input)
                 (setq arguments value)
                 'capture-process)))
      (browser-session-capture
       :url "https://x.com"
       :cookies '("auth_token")
       :output-file "/tmp/browser-session-capture.json"
       :profile-root "/tmp/browser-session-provider-root")
      (should (member "--profile-root" arguments))
      (should (member "/tmp/browser-session-provider-root" arguments))
      (should-not (member "--profile-dir" arguments)))))

(ert-deftest browser-session-builds-capture-arguments ()
  "Capture arguments should preserve provider-selected requirements."
  (let ((browser-session-cdp-port 29999)
        (browser-session-timeout 42)
        (browser-session-proxy nil))
    (should
     (equal
      (browser-session--capture-arguments
       "https://x.com"
       '("auth_token")
       '("ct0")
       t
       "/tmp/session.json"
       "chrome"
       nil
       "/tmp/profile-root"
       nil
       "/tmp/session.js"
       t
       nil)
      '("capture" "--url" "https://x.com" "--output" "/tmp/session.json"
        "--cookie" "auth_token" "--any-cookie" "ct0"
        "--all-origin-cookies" "--script-file" "/tmp/session.js"
        "--restart-running" "--browser" "chrome"
        "--profile-root" "/tmp/profile-root"
        "--port" "29999" "--timeout-secs" "42")))))

(ert-deftest browser-session-builds-explicit-attachment-arguments ()
  "Attaching should omit launch-only profile and proxy settings."
  (let ((browser-session-cdp-port 29999)
        (browser-session-profile-root "/tmp/browser-session-root")
        (browser-session-proxy "http://127.0.0.1:7890")
        arguments)
    (cl-letf (((symbol-function 'browser-session--run-helper)
               (lambda (value _callback _errorback &optional _input)
                 (setq arguments value)
                 'capture-request)))
      (browser-session-capture
       :url "https://x.com"
       :cookies '("auth_token")
       :output-file "/tmp/session.json"
       :attach-existing t)
      (should (member "--attach-existing" arguments))
      (should (member "29999" arguments))
      (should-not (member "--profile-root" arguments))
      (should-not (member "--profile-dir" arguments))
      (should-not (member "--proxy" arguments)))))

(ert-deftest browser-session-allows-page-script-without-cookies ()
  "A provider page result may be the capture's sole completion requirement."
  (let (arguments)
    (cl-letf (((symbol-function 'browser-session--run-helper)
               (lambda (value _callback _errorback &optional _input)
                 (setq arguments value)
                 'capture-request)))
      (should
       (eq
        (browser-session-capture
         :url "https://example.com/challenge"
         :output-file "/tmp/browser-session-page.json"
         :script-file "/tmp/provider.js")
        'capture-request))
      (should (member "--script-file" arguments))
      (should-not (member "--cookie" arguments))
      (should-not (member "--any-cookie" arguments)))))

(ert-deftest browser-session-sends-provider-script-through-stdin ()
  "A direct provider script should use stdin rather than argv or a file."
  (let (arguments input)
    (cl-letf (((symbol-function 'browser-session--run-helper)
               (lambda (value _callback _errorback &optional received-input)
                 (setq arguments value
                       input received-input)
                 'capture-request)))
      (browser-session-capture
       :url "https://example.com/challenge"
       :output-file "/tmp/browser-session-page.json"
       :script "(() => null)()")
      (should (member "--script-stdin" arguments))
      (should-not (member "--script-file" arguments))
      (should (equal input "(() => null)()")))))

(ert-deftest browser-session-rejects-multiple-provider-script-sources ()
  "Direct and file page scripts are a closed mutually exclusive choice."
  (should-error
   (browser-session-capture
    :url "https://example.com/challenge"
    :output-file "/tmp/browser-session-page.json"
    :script "(() => null)()"
    :script-file "/tmp/provider.js")
   :type 'user-error))

(ert-deftest browser-session-rejects-capture-without-completion-requirement ()
  "A capture should not complete from URL navigation alone."
  (should-error
   (browser-session-capture
    :url "https://example.com"
    :output-file "/tmp/browser-session-page.json")
   :type 'user-error))

(ert-deftest browser-session-builds-profile-preparation-arguments ()
  "Profile preparation should not receive capture-only options."
  (let ((browser-session-timeout 60)
        (browser-session-proxy "http://127.0.0.1:7890"))
    (should
     (equal
      (browser-session--prepare-profile-arguments
       "https://music.youtube.com" "firefox" "/tmp/profile" nil)
      '("prepare-profile" "--url" "https://music.youtube.com"
        "--timeout-secs" "60" "--browser" "firefox"
        "--profile-dir" "/tmp/profile")))))

(ert-deftest browser-session-decodes-success-envelope ()
  "A supported helper envelope should return its data alist."
  (should
   (equal
    (browser-session--decode-envelope
     (json-encode
      '((ok . t) (schema . 1) (protocol . 1)
        (data . ((browser . "chrome") (output . "/tmp/session.json"))))))
    '((browser . "chrome") (output . "/tmp/session.json")))))

(ert-deftest browser-session-decodes-structured-helper-errors ()
  "A helper error should preserve its stable code and message."
  (let (payload)
    (condition-case error
        (browser-session--decode-envelope
         (json-encode
          '((ok . nil) (schema . 1) (protocol . 1)
            (error . ((code . "browser-restart-required")
                      (message . "Close the browser")
                      (retryable . nil)
                      (auth-required . nil))))))
      (browser-session-error
       (setq payload (browser-session--error-payload error))))
    (should (equal (browser-session-error-code payload)
                   "browser-restart-required"))
    (should (equal (browser-session-error-message payload)
                   "Close the browser"))))

(ert-deftest browser-session-capture-delivers-structured-process-errors ()
  "The asynchronous capture path should preserve helper error codes."
  (skip-unless (executable-find "sh"))
  (let ((script (make-temp-file "browser-session-helper-" nil ".sh"))
        (browser-session-helper-command nil)
        received)
    (unwind-protect
        (progn
          (with-temp-file script
            (insert "#!/bin/sh\n"
                    "printf '%s\\n' '{\"ok\":false,\"schema\":1,\"protocol\":1,\"error\":{\"code\":\"browser-restart-required\",\"message\":\"Close the browser\",\"retryable\":false,\"auth-required\":false},\"warnings\":[]}'\n"
                    "printf '%s\\n' 'diagnostic only' >&2\n"
                    "exit 1\n"))
          (set-file-modes script #o700)
          (setq browser-session-helper-command script)
          (browser-session-capture
           :url "https://x.com"
           :cookies '("ct0")
           :output-file "/tmp/browser-session-test.json"
           :errorback (lambda (error)
                        (setq received error)))
          (with-timeout (2 (ert-fail "browser-session helper did not report failure"))
            (while (not received)
              (accept-process-output nil 0.1)))
          (should (equal (browser-session-error-code received)
                         "browser-restart-required"))
          (should (equal (browser-session-error-message received)
                         "Close the browser")))
      (delete-file script))))

(ert-deftest browser-session-capture-delivers-metadata ()
  "The asynchronous capture path should deliver non-secret metadata."
  (skip-unless (executable-find "sh"))
  (let ((script (make-temp-file "browser-session-helper-" nil ".sh"))
        (browser-session-helper-command nil)
        received)
    (unwind-protect
        (progn
          (with-temp-file script
            (insert "#!/bin/sh\n"
                    "printf '%s\\n' '{\"ok\":true,\"schema\":1,\"protocol\":1,\"data\":{\"browser\":\"fake\",\"output\":\"/tmp/session.json\"},\"warnings\":[]}'\n"))
          (set-file-modes script #o700)
          (setq browser-session-helper-command script)
          (browser-session-capture
           :url "https://x.com"
           :cookies '("ct0")
           :output-file "/tmp/browser-session-test.json"
           :callback (lambda (metadata)
                       (setq received metadata)))
          (with-timeout (2 (ert-fail "browser-session helper did not report success"))
            (while (not received)
              (accept-process-output nil 0.1)))
          (should (equal (alist-get 'browser received) "fake")))
      (delete-file script))))

(ert-deftest browser-session-capture-closes-stdin-after-provider-script ()
  "The real process path should deliver the script and a pipe EOF."
  (skip-unless (and (executable-find "sh") (executable-find "cat")))
  (let ((helper (make-temp-file "browser-session-helper-" nil ".sh"))
        (browser-session-helper-command nil)
        received
        failure)
    (unwind-protect
        (progn
          (with-temp-file helper
            (insert
             "#!/bin/sh\n"
             "input=$(cat)\n"
             "if [ \"$input\" != 'provider-expression' ]; then exit 2; fi\n"
             "printf '%s\\n' '{\"ok\":true,\"schema\":1,\"protocol\":1,\"data\":{\"browser\":\"stdin\",\"output\":\"/tmp/session.json\"},\"warnings\":[]}'\n"))
          (set-file-modes helper #o700)
          (setq browser-session-helper-command helper)
          (browser-session-capture
           :url "https://example.com/challenge"
           :script "provider-expression"
           :output-file "/tmp/browser-session-test.json"
           :callback (lambda (metadata) (setq received metadata))
           :errorback (lambda (error) (setq failure error)))
          (with-timeout
              (2 (ert-fail "browser-session helper did not receive stdin EOF"))
            (while (not (or received failure))
              (accept-process-output nil 0.1)))
          (should-not failure)
          (should (equal (alist-get 'browser received) "stdin")))
      (delete-file helper))))

(ert-deftest browser-session-cancel-signals-a-request-once ()
  "Cancellation should interrupt only the helper represented by the request."
  (let ((request (browser-session--request-create))
        signals)
    (setf (browser-session-request-process request) 'fake-process)
    (cl-letf (((symbol-function 'process-live-p) (lambda (_process) t))
              ((symbol-function 'signal-process)
               (lambda (process signal)
                 (push (list process signal) signals))))
      (should (browser-session-cancel request))
      (should-not (browser-session-cancel request))
      (should (browser-session-request-canceled-p request))
      (should (equal signals '((fake-process SIGINT)))))))

(ert-deftest browser-session-reads-private-capture-shape ()
  "A capture file should expose only its structured records to callers."
  (let ((file (make-temp-file "browser-session-test-" nil ".json"))
        (payload
         '((schema . 1)
           (source . ((browser . "chrome") (url . "https://x.com")))
           (cookies . (((name . "ct0") (value . "test-value"))))
           (page . ((ticket . "synthetic")
                    (randstr . "@test")
                    (sid . "123"))))))
    (unwind-protect
        (progn
          (with-temp-file file
            (insert (json-encode payload)))
          (let ((capture (browser-session-read file)))
            (should (equal (alist-get 'browser (alist-get 'source capture))
                           "chrome"))
            (should (equal (alist-get 'name (car (browser-session-cookies capture)))
                           "ct0"))
            (should (equal (alist-get 'sid (browser-session-page capture))
                           "123"))))
      (delete-file file))))

;;; browser-session-test.el ends here
