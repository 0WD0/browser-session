;;; browser-session-test.el --- Tests for browser-session -*- lexical-binding: t; -*-

;; Copyright (C) 2026
;; SPDX-License-Identifier: MIT

;;; Code:

(require 'ert)
(require 'json)
(require 'browser-session)

(ert-deftest browser-session-builds-capture-arguments ()
  "Capture arguments should preserve provider-selected cookie names."
  (let ((browser-session-cdp-port 29999)
        (browser-session-timeout 42)
        (browser-session-proxy nil))
    (should
     (equal
      (browser-session--capture-arguments
       "https://x.com"
       '("auth_token" "ct0")
       "/tmp/session.json"
       "chrome"
       "/tmp/profile"
       nil)
      '("capture" "--url" "https://x.com" "--output" "/tmp/session.json"
        "--cookie" "auth_token" "--cookie" "ct0"
        "--browser" "chrome" "--profile-dir" "/tmp/profile"
        "--port" "29999" "--timeout-secs" "42")))))

(ert-deftest browser-session-capture-uses-custom-browser-settings ()
  "The capture command should use configured browser defaults."
  (let ((browser-session-browser "chromium")
        (browser-session-profile-directory "/tmp/profile"))
    (should
     (equal
      (browser-session--capture-arguments
       "https://x.com" '("ct0") "/tmp/session.json"
       browser-session-browser browser-session-profile-directory nil)
      '("capture" "--url" "https://x.com" "--output" "/tmp/session.json"
        "--cookie" "ct0" "--browser" "chromium"
        "--profile-dir" "/tmp/profile" "--port" "29317"
        "--timeout-secs" "180")))))

(ert-deftest browser-session-decodes-success-envelope ()
  "A supported helper envelope should return its data alist."
  (should
   (equal
    (browser-session--decode-envelope
     (json-encode
      '((ok . t) (schema . 1) (protocol . 1)
        (data . ((browser . "chrome") (output . "/tmp/session.json"))))))
    '((browser . "chrome") (output . "/tmp/session.json")))))

(ert-deftest browser-session-rejects-unsupported-envelope ()
  "An unsupported helper envelope should not be accepted."
  (should-error
   (browser-session--decode-envelope
    (json-encode '((ok . t) (schema . 9) (protocol . 1) (data . nil))))))

(ert-deftest browser-session-reads-private-capture-shape ()
  "A capture file should expose only its structured records to callers."
  (let ((file (make-temp-file "browser-session-test-" nil ".json")))
    (unwind-protect
        (progn
          (with-temp-file file
            (insert
             (json-encode
              `((schema . 1)
                (source . ((browser . "chrome") (url . "https://x.com")))
                (cookies . (((name . "ct0")
                             (value . ,(format "test-%s" (emacs-pid))))))))))
          (let ((capture (browser-session-read file)))
            (should (equal (alist-get 'browser (alist-get 'source capture))
                           "chrome"))
            (should (equal (alist-get 'name (car (browser-session-cookies capture)))
                           "ct0"))))
      (delete-file file))))

;;; browser-session-test.el ends here
