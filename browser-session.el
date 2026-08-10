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

(defcustom browser-session-helper-command
  (expand-file-name "target/debug/browser-session"
                    (file-name-directory
                     (or load-file-name buffer-file-name default-directory)))
  "External helper executable used to capture browser sessions."
  :type 'file
  :group 'browser-session)

(defcustom browser-session-browser nil
  "Browser executable or supported browser name used for capture.
When nil, the helper discovers an available Chromium-compatible browser."
  :type '(choice (const :tag "Auto" nil)
                 string)
  :group 'browser-session)

(defcustom browser-session-profile-directory nil
  "Optional Chromium user-data directory used for capture.
This is the user-data directory root, not its `Default/' child profile."
  :type '(choice (const :tag "Let the helper decide" nil)
                 directory)
  :group 'browser-session)

(defcustom browser-session-cdp-port 29317
  "Local Chromium DevTools port used for capture."
  :type '(restricted-sexp
          :match-alternatives
          ((lambda (value)
             (and (integerp value) (<= 1 value 65535)))))
  :group 'browser-session)

(defcustom browser-session-timeout 180
  "Seconds the helper waits for the requested browser session."
  :type '(restricted-sexp
          :match-alternatives
          ((lambda (value)
             (and (integerp value) (> value 0)))))
  :group 'browser-session)

(defcustom browser-session-proxy nil
  "Optional proxy URL passed to a browser that the helper starts."
  :type '(choice (const :tag "No proxy" nil)
                 string)
  :group 'browser-session)

(defconst browser-session--schema-version 1
  "Capture-file schema version accepted by this client.")

(defconst browser-session--protocol-version 1
  "Helper protocol version accepted by this client.")

(defun browser-session--nonblank-string (value)
  "Return VALUE when it is a nonblank string, otherwise nil."
  (and (stringp value)
       (not (string-blank-p value))
       value))

(defun browser-session--helper-arguments (&optional browser profile-directory)
  "Return helper options for BROWSER and PROFILE-DIRECTORY."
  (append
   (when-let* ((value (browser-session--nonblank-string browser)))
     (list "--browser" value))
   (when-let* ((value (browser-session--nonblank-string profile-directory)))
     (list "--profile-dir" (expand-file-name value)))
   (list "--port" (number-to-string browser-session-cdp-port)
         "--timeout-secs" (number-to-string browser-session-timeout))
   (when-let* ((value (browser-session--nonblank-string browser-session-proxy)))
     (list "--proxy" value))))

(defun browser-session--capture-arguments
    (url cookies output-file browser profile-directory script-file)
  "Return helper arguments for URL, COOKIES, and OUTPUT-FILE.
BROWSER, PROFILE-DIRECTORY, and SCRIPT-FILE provide optional capture settings."
  (append
   (list "capture" "--url" url "--output" (expand-file-name output-file))
   (cl-mapcan (lambda (cookie) (list "--cookie" cookie)) cookies)
   (when-let* ((value (browser-session--nonblank-string script-file)))
     (list "--script-file" (expand-file-name value)))
   (browser-session--helper-arguments browser profile-directory)))

(defun browser-session--decode-envelope (text)
  "Decode helper JSON TEXT and return its metadata data alist."
  (let ((envelope
         (condition-case error
             (json-parse-string text
                                :object-type 'alist
                                :array-type 'list
                                :null-object nil
                                :false-object nil)
           (json-parse-error
            (error "Browser-session returned invalid JSON: %s"
                   (error-message-string error))))))
    (unless (and (equal (alist-get 'schema envelope) browser-session--schema-version)
                 (equal (alist-get 'protocol envelope)
                        browser-session--protocol-version))
      (error "Browser-session returned an unsupported protocol envelope"))
    (if (alist-get 'ok envelope)
        (or (alist-get 'data envelope)
            (error "Browser-session returned no data"))
      (let* ((payload (alist-get 'error envelope))
             (message (or (alist-get 'message payload)
                          "Browser-session failed")))
        (error "%s" message)))))

(defun browser-session--process-result (stdout-buffer stderr-buffer)
  "Return parsed output from STDOUT-BUFFER and STDERR-BUFFER."
  (let ((stdout (with-current-buffer stdout-buffer (string-trim (buffer-string))))
        (stderr (with-current-buffer stderr-buffer (string-trim (buffer-string)))))
    (condition-case error
        (browser-session--decode-envelope stdout)
      (error
       (if (string-empty-p stderr)
           (signal (car error) (cdr error))
         (error "%s" stderr))))))

;;;###autoload
(cl-defun browser-session-capture
    (&key url cookies output-file browser profile-directory script-file
     callback errorback)
  "Capture selected COOKIES for URL into OUTPUT-FILE asynchronously.
CALLBACK receives non-secret helper metadata after a successful capture.
ERRORBACK receives one human-readable error string after failure.  URL,
COOKIES, and OUTPUT-FILE are required."
  (unless (browser-session--nonblank-string url)
    (user-error "A browser-session URL is required"))
  (unless (and (listp cookies)
               cookies
               (cl-every (lambda (cookie)
                           (browser-session--nonblank-string cookie))
                         cookies))
    (user-error "At least one browser-session cookie name is required"))
  (unless (browser-session--nonblank-string output-file)
    (user-error "A private browser-session output file is required"))
  (unless (file-executable-p browser-session-helper-command)
    (user-error "Browser-session helper is not executable: %s"
                browser-session-helper-command))
  (let* ((stdout-buffer (generate-new-buffer " *browser-session-stdout*"))
         (stderr-buffer (generate-new-buffer " *browser-session-stderr*"))
         (arguments (browser-session--capture-arguments
                     url cookies output-file
                     (or browser browser-session-browser)
                     (or profile-directory browser-session-profile-directory)
                     script-file))
         (process-name (format "browser-session-%s" (make-temp-name "capture-"))))
    (make-process
     :name process-name
     :command (cons browser-session-helper-command arguments)
     :buffer stdout-buffer
     :stderr stderr-buffer
     :noquery t
     :sentinel
     (lambda (process _event)
       (when (memq (process-status process) '(exit signal))
         (unwind-protect
             (if (zerop (process-exit-status process))
                 (condition-case error
                     (when callback
                       (funcall callback
                                (browser-session--process-result
                                 stdout-buffer stderr-buffer)))
                   (error
                    (if errorback
                        (funcall errorback (error-message-string error))
                      (message "%s" (error-message-string error)))))
               (let ((message
                      (condition-case error
                          (browser-session--process-result
                           stdout-buffer stderr-buffer)
                        (error (error-message-string error)))))
                 (if errorback
                     (funcall errorback message)
                   (message "%s" message))))
           (kill-buffer stdout-buffer)
           (kill-buffer stderr-buffer)))))))

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
The returned value contains session material and must not be persisted or
included in diagnostics."
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

(provide 'browser-session)

;;; browser-session.el ends here
