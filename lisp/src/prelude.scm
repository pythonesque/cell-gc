;; I stole these lines of code from <https://www.bluishcoder.co.nz/jsscheme/>.
;; Original by Alex Yakovlev. Adapted by Chris Double.

(define (list . x) x)
(define (not x) (if x #f #t))
(define (negative? x) (< x 0))
(define (positive? x) (> x 0))
(define (zero? x) (= x 0))
(define (abs x) (if (< x 0) (- x) x))
(define magnitude abs)
;
(define (char-ci=?  x y) (char=?  (char-downcase x) (char-downcase y)))
(define (char-ci>?  x y) (char>?  (char-downcase x) (char-downcase y)))
(define (char-ci<?  x y) (char<?  (char-downcase x) (char-downcase y)))
(define (char-ci>=? x y) (char>=? (char-downcase x) (char-downcase y)))
(define (char-ci<=? x y) (char<=? (char-downcase x) (char-downcase y)))
;
(define (map f ls . more)
  (define (map1 l)
    (if (null? l)
      '()
      (if (pair? l)
          (cons (f (car l)) (map1 (cdr l)))
          (f l))))
  (define (map-more l m)
    (if (null? l)
        '()
        (if (pair? l)
            (cons (apply f (car l) (map car m))
                  (map-more (cdr l)
                            (map cdr m)))
            (apply f l m))))
  (if (null? more)
      (map1 ls)
      (map-more ls more)))
; tail-recursive map
(define (map+ f . lst)
  (define r '())
  (define o #f)
  (define p #f)
  (define (map-lst op l)
    (if (pair? l) (cons (op (car l)) (map-lst op (cdr l))) '()))
  (define (do-map)
    (if (pair? (car lst)) (begin
          (set! o (cons (apply f (map car lst)) '()))
          (if (null? r) (set! r o) (set-cdr! p o))
          (set! p o)
          (set! lst (map cdr lst))
          (do-map))
      (if (not (null? (car lst)))
         (if p (set-cdr! p (apply f lst))
               (set! r (apply f lst))))))
  (do-map) r)
;
(define (caar x) (car (car x)))
(define (cadr x) (car (cdr x)))
(define (cdar x) (cdr (car x)))
(define (cddr x) (cdr (cdr x)))
;
(define (caaar x) (car (car (car x))))
(define (caadr x) (car (car (cdr x))))
(define (cadar x) (car (cdr (car x))))
(define (caddr x) (car (cdr (cdr x))))
(define (cdaar x) (cdr (car (car x))))
(define (cdadr x) (cdr (car (cdr x))))
(define (cddar x) (cdr (cdr (car x))))
(define (cdddr x) (cdr (cdr (cdr x))))
;
(define (caaddr x) (car (car (cdr (cdr x)))))
(define (cadddr x) (car (cdr (cdr (cdr x)))))
(define (cdaddr x) (cdr (car (cdr (cdr x)))))
(define (cddddr x) (cdr (cdr (cdr (cdr x)))))
;
(define (length lst . x)
  (define l (if (null? x) 0 (car x)))
  (if (pair? lst) (length (cdr lst) (+ l 1)) l))
(define (length+ lst . x)
  (define l (if (null? x) 0 (car x)))
  (if (null? lst) l
      (if (pair? lst) (length+ (cdr lst) (+ l 1)) (+ l 1))))

(define (list-ref lst n)
  (if (= n 0) (car lst) (list-ref (cdr lst) (- n 1))))
(define (list-tail lst n)
  (if (= n 0) lst (list-tail (cdr lst) (- n 1))))
(define (reverse lst . l2)
  (define r (if (null? l2) l2 (car l2)))
  (if (null? lst) r
      (reverse (cdr lst) (cons (car lst) r))))
;
(define (append l1 . more)
  (if (null? more) l1
      (if (null? l1)
          (apply append more)
          (cons (car l1)
                (apply append (cdr l1) more)))))
;
(define (memq+ x ls)
  (if (pair? ls)
      (if (eq? (car ls) x) ls
          (memq+ x (cdr ls)))
      (if (eq? x ls) ls #f)))
(define memq memq+)
(define (memv x ls)
  (if (pair? ls)
      (if (eqv? (car ls) x) ls
          (memv x (cdr ls)))
  (if (eqv? x ls) ls #f)))
(define (member x ls)
  (if (pair? ls)
      (if (equal? (car ls) x) ls
          (member x (cdr ls)))
  (if (equal? x ls) ls #f)))
;
(define (assq x ls)
  (if (null? ls) #f
      (if (eq? (caar ls) x) (car ls)
          (assq x (cdr ls)))))
(define (assv x ls)
  (if (null? ls) #f
      (if (eqv? (caar ls) x) (car ls)
          (assv x (cdr ls)))))
(define (assoc x ls)
  (if (null? ls) #f
      (if (equal? (caar ls) x) (car ls)
          (assoc x (cdr ls)))))
;
(define list?
  ((lambda ()
    (define (race h t)
      (if (pair? h)
          ((lambda (h)
             (if (pair? h)
                 (if (not (eq? h t))
                     (race (cdr h) (cdr t))
                     #f)
                 (null? h))) (cdr h))
          (null? h)))
    (lambda (x) (race x x)))))
;
(define equal?
  (lambda (x y)
    ((lambda (eqv)
       (if eqv eqv
           (if (pair? x)
               (if (pair? y)
                   (if (equal? (car x) (car y))
                       (equal? (cdr x) (cdr y))
                       #f)
                   #f)
               (if (vector? x)
                   (if (vector? y)
                       ((lambda (n)
                          (if (= (vector-length y) n)
                              ((letrec ((loop
                                         (lambda (i)
                                           ((lambda (eq-len)
                                              (if eq-len
                                                  eq-len
                                                  (if (equal? (vector-ref x i)
                                                              (vector-ref y i))
                                                      (loop (+ i 1))
                                                      #f)))
                                            (= i n)))))
                                 loop)
                               0)
                              #f))
                        (vector-length x))
                       #f)
                   (if (string? x)
                       (if (string? y)
                           (equal? (string->list x) (string->list y))
                           #f)
                       #f)))))
     (eqv? x y))))
;
(define (for-each f . lst)
  (if (not (null? (car lst))) (begin
      (apply f (map+ car lst))
      (apply for-each f (map+ cdr lst)))))
;
(define (vector-fill! v obj)
  (define l (vector-length v))
  (define (vf i) (if (< i l) (begin (vector-set! v i obj) (vf (+ i 1)))))
  (vf 0))
(define (vector->list v)
  (define (loop i l)
    (if (< i 0)
        l
        (loop (- i 1) (cons (vector-ref v i) l))))
  (loop (- (vector-length v) 1) '()))
;
(define dynamic-wind #f)
((lambda ()

  (define winders '())

  (define (common-tail x y)
     (define lx (length x))
     (define ly (length y))
     (define (loop x y)
       (if (eq? x y)
           x
           (loop (cdr x) (cdr y))))
     (loop (if (> lx ly) (list-tail x (- lx ly)) x)
           (if (> ly lx) (list-tail y (- ly lx)) y)))

  (define (do-wind new)
    (define tail (common-tail new winders))
    (define (f1 l)
      (if (not (eq? l tail))
          (begin
            (set! winders (cdr l))
            ((cdar l))
            (f1 (cdr l)))))
    (define (f2 l)
      (if (not (eq? l tail))
          (begin
            (f2 (cdr l))
            ((caar l))
            (set! winders l))))
    (f1 winders)
    (f2 new))

  ((lambda (c)
    (set! call/cc
      (lambda (f)
        (c (lambda (k)
             (f ((lambda (save)
                  (lambda x
                    (if (not (eq? save winders)) (do-wind save))
                    (apply k x)))
                 winders)))))))
      call/cc)
  (set! call-with-current-continuation call/cc)

  (set! dynamic-wind
    (lambda (in body out)
      (define ans #f)
      (in)
      (set! winders (cons (cons in out) winders))
      (set! ans (body))
      (set! winders (cdr winders))
      (out)
      ans))))
;
(define void
  (letrec ((unspecified (if #f #f)))
    (lambda () unspecified)))
