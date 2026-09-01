;;; browser-session.el --- Capture browser sessions -*- lexical-binding: t; -*-

;; Author: 0WD0 <me@0wd0.com>
;; URL: https://github.com/0WD0/browser-session
;; Version: 0.1.0
;; Package-Requires: ((emacs "29.1"))
;; Keywords: comm, convenience
;; SPDX-License-Identifier: MIT

;;; Commentary:

;; Start the provider-neutral `browser-session' helper and read its private
;; versioned capture files.  Provider packages own cookie allowlists and
;; credential validation.

;;; Code:

(require 'cl-lib)
(require 'json)
(require 'subr-x)

(defgroup browser-session nil
  "Capture authenticated browser sessions."
  :group 'comm)

(defun browser-session--helper-executable-name ()
  "Return the browser-session helper executable name for this platform."
  (if (eq system-type 'windows-nt)
      "browser-session.exe"
    "browser-session"))

(defun browser-session--default-helper-command (&optional directory)
  "Return the default helper path for DIRECTORY or the loaded package.

The source-tree candidate lets a straight build use its symlinked source
checkout, whose Rust target directory is not copied into the build directory.
An executable found on variable `exec-path' is the fallback for installed
packages."
  (let* ((directory (or directory
                        (file-name-directory
                         (or load-file-name buffer-file-name default-directory))))
         (name (browser-session--helper-executable-name))
         (loaded-el (expand-file-name "browser-session.el" directory))
         (source-directory
          (when (file-exists-p loaded-el)
            (file-name-directory (file-truename loaded-el))))
         (candidates
          (delete-dups
           (delq nil
                 (list (and source-directory
                            (expand-file-name (concat "target/debug/" name)
                                              source-directory))
                       (expand-file-name (concat "target/debug/" name) directory)
                       (executable-find name))))))
    (or (cl-find-if #'file-executable-p candidates)
        (car candidates))))

(defcustom browser-session-helper-command
  (browser-session--default-helper-command)
  "External helper executable used to capture browser sessions."
  :type 'file
  :group 'browser-session)

(defcustom browser-session-browser nil
  "Browser executable or supported browser name used for capture.
When nil, the helper uses the system default supported browser."
  :type '(choice (const :tag "Auto" nil)
                 string)
  :group 'browser-session)

(defcustom browser-session-profile-directory nil
  "Optional isolated profile directory used for capture.
For Chromium this is the user-data directory root.  For Firefox and Zen it is
the profile directory itself."
  :type '(choice (const :tag "Let the helper decide" nil)
                 directory)
  :group 'browser-session)

(defcustom browser-session-profile-root
  (locate-user-emacs-file "browser-session/")
  "Root for automatic Chrome and Firefox-family profiles.

When no explicit profile directory is supplied, the helper uses
`login-profile', `login-profile-firefox', or `login-profile-zen' below this
root for Chrome, Firefox, or Zen respectively.  Set this to nil to use a
browser's normal profile instead."
  :type '(choice (const :tag "Use the browser's normal profile" nil)
                 directory)
  :group 'browser-session)

(defcustom browser-session-cdp-port 0
  "Local browser remote-control port used for capture.
Zero asks the helper to allocate an ephemeral loopback port.  Attaching to an
existing endpoint requires an explicit nonzero value."
  :type '(restricted-sexp
          :match-alternatives
          ((lambda (value)
             (and (integerp value) (<= 0 value 65535)))))
  :group 'browser-session)

(defcustom browser-session-timeout 180
  "Seconds the helper waits for requested browser state."
  :type '(restricted-sexp
          :match-alternatives
          ((lambda (value)
             (and (integerp value) (> value 0)))))
  :group 'browser-session)

(defcustom browser-session-proxy nil
  "Optional proxy URL passed to a Chromium browser started by the helper."
  :type '(choice (const :tag "No proxy" nil)
                 string)
  :group 'browser-session)

(defconst browser-session--schema-version 1
  "Capture-file schema version accepted by this client.")

(defconst browser-session--protocol-version 1
  "Helper protocol version accepted by this client.")

(define-error 'browser-session-error "Browser-session error")

(cl-defstruct (browser-session-request
               (:constructor browser-session--request-create))
  "One asynchronous browser-session helper request."
  process
  canceled-p)

(defun browser-session--nonblank-string (value)
  "Return VALUE when it is a nonblank string, otherwise nil."
  (and (stringp value)
       (not (string-blank-p value))
       value))

(defun browser-session--cookie-list-p (value)
  "Return non-nil when VALUE is a list of nonblank cookie names."
  (and (listp value)
       (cl-every #'browser-session--nonblank-string value)))

(defun browser-session--helper-arguments
    (&optional browser profile-directory profile-root attach-existing)
  "Return helper options for BROWSER, PROFILE-DIRECTORY, and PROFILE-ROOT.
When ATTACH-EXISTING is non-nil, omit launch-only profile and proxy options."
  (append
   (when-let* ((value (browser-session--nonblank-string browser)))
     (list "--browser" value))
   (unless attach-existing
     (append
      (when-let* ((value (browser-session--nonblank-string profile-directory)))
        (list "--profile-dir" (expand-file-name value)))
      (when-let* ((value (browser-session--nonblank-string profile-root)))
        (list "--profile-root" (expand-file-name value)))))
   (list "--port" (number-to-string browser-session-cdp-port)
         "--timeout-secs" (number-to-string browser-session-timeout))
   (unless attach-existing
     (when-let* ((value (browser-session--nonblank-string browser-session-proxy)))
       (list "--proxy" value)))))

(defun browser-session--capture-arguments
    (url cookies any-cookies all-origin-cookies output-file browser
         profile-directory profile-root script script-file restart-running
         attach-existing)
  "Return capture arguments for URL, cookie requirements, and OUTPUT-FILE.
COOKIES are individually required; ANY-COOKIES require any one listed name.
When ALL-ORIGIN-COOKIES is non-nil, capture every unexpired cookie scoped to
URL.  BROWSER, PROFILE-DIRECTORY, PROFILE-ROOT, SCRIPT, SCRIPT-FILE,
RESTART-RUNNING, and ATTACH-EXISTING provide optional capture settings."
  (append
   (list "capture" "--url" url "--output" (expand-file-name output-file))
   (cl-mapcan (lambda (cookie) (list "--cookie" cookie)) cookies)
   (cl-mapcan (lambda (cookie) (list "--any-cookie" cookie)) any-cookies)
   (when all-origin-cookies
     (list "--all-origin-cookies"))
   (when script
     (list "--script-stdin"))
   (when-let* ((value (browser-session--nonblank-string script-file)))
     (list "--script-file" (expand-file-name value)))
   (when restart-running
     (list "--restart-running"))
   (when attach-existing
     (list "--attach-existing"))
   (browser-session--helper-arguments
    browser profile-directory profile-root attach-existing)))

(defun browser-session--effective-profile-settings
    (profile-directory profile-root)
  "Return effective PROFILE-DIRECTORY and PROFILE-ROOT settings.

Explicit settings override browser-session defaults.  A profile directory
always takes precedence over an automatic profile root."
  (when (and (browser-session--nonblank-string profile-directory)
             (browser-session--nonblank-string profile-root))
    (user-error "Use either a browser-session profile directory or profile root"))
  (cond
   ((browser-session--nonblank-string profile-directory)
    (cons profile-directory nil))
   ((browser-session--nonblank-string profile-root)
    (cons nil profile-root))
   ((browser-session--nonblank-string browser-session-profile-directory)
    (cons browser-session-profile-directory nil))
   (t
    (cons nil browser-session-profile-root))))

(defun browser-session--prepare-profile-arguments
    (url browser profile-directory profile-root)
  "Return profile-preparation arguments for URL.
BROWSER, PROFILE-DIRECTORY, and PROFILE-ROOT provide browser settings."
  (append
   (list "prepare-profile" "--url" url
         "--timeout-secs" (number-to-string browser-session-timeout))
   (when-let* ((value (browser-session--nonblank-string browser)))
     (list "--browser" value))
   (when-let* ((value (browser-session--nonblank-string profile-directory)))
     (list "--profile-dir" (expand-file-name value)))
   (when-let* ((value (browser-session--nonblank-string profile-root)))
     (list "--profile-root" (expand-file-name value)))))

(defun browser-session--error-data (code message &optional retryable auth-required)
  "Return a structured error with CODE and MESSAGE.
RETRYABLE and AUTH-REQUIRED are optional error flags."
  `((code . ,code)
    (message . ,message)
    (retryable . ,retryable)
    (auth-required . ,auth-required)))

(defun browser-session--signal-error (error)
  "Signal ERROR as a structured `browser-session-error'."
  (signal 'browser-session-error (list error)))

(defun browser-session--error-payload (error)
  "Return a structured payload from signaled ERROR."
  (if (and (eq (car-safe error) 'browser-session-error)
           (listp (cadr error)))
      (cadr error)
    (browser-session--error-data
     "process-failure"
     (error-message-string error))))

;;;###autoload
(defun browser-session-error-code (error)
  "Return the stable browser-session error code from ERROR."
  (alist-get 'code error))

;;;###autoload
(defun browser-session-error-message (error)
  "Return the human-readable browser-session error message from ERROR."
  (or (alist-get 'message error) "Browser-session failed"))

(defun browser-session--decode-envelope (text)
  "Decode helper JSON TEXT and return its metadata data alist.
Signals `browser-session-error' with a structured payload for helper failure."
  (let ((envelope
         (condition-case error
             (json-parse-string text
                                :object-type 'alist
                                :array-type 'list
                                :null-object nil
                                :false-object nil)
           (json-parse-error
            (browser-session--signal-error
             (browser-session--error-data
              "process-failure"
              (format "Browser-session returned invalid JSON: %s"
                      (error-message-string error))))))))
    (unless (and (equal (alist-get 'schema envelope) browser-session--schema-version)
                 (equal (alist-get 'protocol envelope)
                        browser-session--protocol-version))
      (browser-session--signal-error
       (browser-session--error-data
        "unsupported-protocol"
        "Browser-session returned an unsupported protocol envelope")))
    (if (alist-get 'ok envelope)
        (or (alist-get 'data envelope)
            (browser-session--signal-error
             (browser-session--error-data
              "process-failure" "Browser-session returned no data")))
      (let ((payload (alist-get 'error envelope)))
        (unless (and (listp payload)
                     (stringp (alist-get 'code payload))
                     (stringp (alist-get 'message payload)))
          (browser-session--signal-error
           (browser-session--error-data
            "process-failure" "Browser-session returned an invalid error envelope")))
        (browser-session--signal-error payload)))))

(defun browser-session--process-result (stdout-buffer stderr-buffer)
  "Return parsed output from STDOUT-BUFFER and STDERR-BUFFER."
  (let ((stdout (with-current-buffer stdout-buffer (string-trim (buffer-string))))
        (stderr (with-current-buffer stderr-buffer (string-trim (buffer-string)))))
    (condition-case error
        (browser-session--decode-envelope stdout)
      (browser-session-error
       (signal (car error) (cdr error)))
      (error
       (browser-session--signal-error
        (browser-session--error-data
         "process-failure"
         (if (string-empty-p stderr)
             (error-message-string error)
           stderr)))))))

(defun browser-session--deliver-error (errorback error)
  "Call ERRORBACK with structured ERROR, or display its message."
  (if errorback
      (funcall errorback error)
    (message "%s" (browser-session-error-message error))))

(defun browser-session--run-helper (arguments callback errorback &optional input)
  "Run helper with ARGUMENTS asynchronously.
CALLBACK receives successful metadata.  ERRORBACK receives structured helper
error data.  Send optional INPUT to the helper's standard input and close it.
Return a `browser-session-request' that can be cancelled."
  (unless (file-executable-p browser-session-helper-command)
    (user-error "Browser-session helper is not executable: %s"
                browser-session-helper-command))
  (let* ((stdout-buffer (generate-new-buffer " *browser-session-stdout*"))
         (stderr-buffer (generate-new-buffer " *browser-session-stderr*"))
         (process-name
          (format "browser-session-%s" (make-temp-name "capture-")))
         (request (browser-session--request-create))
         (process
          (make-process
           :name process-name
           :command (cons browser-session-helper-command arguments)
           :buffer stdout-buffer
           :connection-type 'pipe
           :stderr stderr-buffer
           :noquery t
           :coding 'utf-8-unix
           :sentinel
           (lambda (finished-process _event)
             (when (memq (process-status finished-process) '(exit signal))
               (unwind-protect
                   (condition-case error
                       (if (zerop (process-exit-status finished-process))
                           (when callback
                             (funcall callback
                                      (browser-session--process-result
                                       stdout-buffer stderr-buffer)))
                         (progn
                           (browser-session--process-result
                            stdout-buffer stderr-buffer)
                           (browser-session--deliver-error
                            errorback
                            (browser-session--error-data
                             "process-failure"
                             (format "Browser-session exited with status %d"
                                     (process-exit-status
                                      finished-process))))))
                     (error
                      (browser-session--deliver-error
                       errorback (browser-session--error-payload error))))
                 (setf (browser-session-request-process request) nil)
                 (kill-buffer stdout-buffer)
                 (kill-buffer stderr-buffer)))))))
    (setf (browser-session-request-process request) process)
    (when input
      (process-send-string process input)
      (process-send-eof process))
    request))

;;;###autoload
(defun browser-session-cancel (request)
  "Cancel asynchronous browser-session REQUEST cooperatively.
REQUEST must be a live value returned by `browser-session-capture' or
`browser-session-prepare-profile'.  Its error callback receives the stable
`cancelled' error after the helper has closed any isolated browser it started."
  (unless (browser-session-request-p request)
    (user-error "Invalid browser-session request"))
  (unless (browser-session-request-canceled-p request)
    (setf (browser-session-request-canceled-p request) t)
    (when-let* ((process (browser-session-request-process request))
                ((process-live-p process)))
      (signal-process process 'SIGINT))
    t))

;;;###autoload
(cl-defun browser-session-capture
    (&key url cookies any-cookies all-origin-cookies output-file browser
          profile-directory profile-root script script-file restart-running
          attach-existing callback errorback)
  "Capture selected browser session fields asynchronously.
URL and OUTPUT-FILE are required.  COOKIES names fields that must all exist;
ANY-COOKIES names an alternative group for which one field must exist.  SCRIPT
contains a provider-owned JavaScript expression sent through standard input;
SCRIPT-FILE names an expression file.  They are mutually exclusive, and either
may be the sole completion requirement.  ALL-ORIGIN-COOKIES requests every
unexpired cookie scoped to URL.  BROWSER, PROFILE-DIRECTORY, PROFILE-ROOT, and
RESTART-RUNNING configure a browser launched by the helper.  ATTACH-EXISTING
explicitly attaches to the nonzero `browser-session-cdp-port' and does not use
profile or proxy settings.  CALLBACK receives non-secret helper metadata after
success.  ERRORBACK receives a structured error alist after failure.  Return a
cancellable `browser-session-request'."
  (unless (browser-session--nonblank-string url)
    (user-error "A browser-session URL is required"))
  (unless (and (browser-session--cookie-list-p cookies)
               (browser-session--cookie-list-p any-cookies))
    (user-error "Browser-session cookie names must be nonblank strings"))
  (unless (or (null script) (browser-session--nonblank-string script))
    (user-error "A browser-session page script must be a nonblank string"))
  (when (and script (browser-session--nonblank-string script-file))
    (user-error "Use either a browser-session script string or script file"))
  (unless (or cookies any-cookies
              (browser-session--nonblank-string script)
              (browser-session--nonblank-string script-file))
    (user-error "At least one cookie requirement or page script is required"))
  (unless (browser-session--nonblank-string output-file)
    (user-error "A private browser-session output file is required"))
  (when (and attach-existing (zerop browser-session-cdp-port))
    (user-error "Attaching requires a nonzero browser-session control port"))
  (pcase-let ((`(,effective-profile-directory . ,effective-profile-root)
               (if attach-existing
                   (cons nil nil)
                 (browser-session--effective-profile-settings
                  profile-directory profile-root))))
    (browser-session--run-helper
     (browser-session--capture-arguments
      url cookies any-cookies all-origin-cookies output-file
      (or browser browser-session-browser)
      effective-profile-directory
      effective-profile-root
      script script-file restart-running attach-existing)
     callback errorback script)))

;;;###autoload
(cl-defun browser-session-prepare-profile
    (&key url browser profile-directory profile-root callback errorback)
  "Prepare an isolated Firefox-family browser profile for URL asynchronously.
BROWSER selects Firefox or Zen.  PROFILE-DIRECTORY overrides PROFILE-ROOT.
CALLBACK receives non-secret profile metadata after the user closes the browser.
ERRORBACK receives a structured error alist after failure."
  (unless (browser-session--nonblank-string url)
    (user-error "A browser-session URL is required"))
  (pcase-let ((`(,effective-profile-directory . ,effective-profile-root)
               (browser-session--effective-profile-settings
                profile-directory profile-root)))
    (browser-session--run-helper
     (browser-session--prepare-profile-arguments
      url
      (or browser browser-session-browser)
      effective-profile-directory
      effective-profile-root)
     callback errorback)))

(defun browser-session--decode-capture (text)
  "Decode and validate a private capture file containing TEXT."
  (let ((capture
         (condition-case error
             (json-parse-string text
                                :object-type 'alist
                                :array-type 'list
                                :null-object nil
                                :false-object nil)
           (json-parse-error
            (error "Browser-session capture is invalid JSON: %s"
                   (error-message-string error))))))
    (unless (equal (alist-get 'schema capture) browser-session--schema-version)
      (error "Browser-session capture has an unsupported schema"))
    (unless (and (listp (alist-get 'source capture))
                 (listp (alist-get 'cookies capture)))
      (error "Browser-session capture has an invalid shape"))
    capture))

;;;###autoload
(defun browser-session-read (file)
  "Read and validate private capture FILE.
The returned value can contain session material.  Callers must avoid persisting
it or including it in diagnostics."
  (unless (and (stringp file) (file-readable-p file))
    (user-error "Browser-session capture is not readable: %s" file))
  (browser-session--decode-capture
   (with-temp-buffer
     (insert-file-contents-literally file)
     (buffer-string))))

;;;###autoload
(defun browser-session-cookies (capture)
  "Return cookie records from validated CAPTURE.
CAPTURE should be the value returned by `browser-session-read'."
  (unless (and (listp capture)
               (equal (alist-get 'schema capture) browser-session--schema-version))
    (user-error "Invalid browser-session capture"))
  (alist-get 'cookies capture))

;;;###autoload
(defun browser-session-page (capture)
  "Return the provider-owned page result from validated CAPTURE.
CAPTURE should be the value returned by `browser-session-read'.  The result may
contain private session material and must not be persisted or diagnosed."
  (unless (and (listp capture)
               (equal (alist-get 'schema capture) browser-session--schema-version))
    (user-error "Invalid browser-session capture"))
  (alist-get 'page capture))

(provide 'browser-session)

;;; browser-session.el ends here
