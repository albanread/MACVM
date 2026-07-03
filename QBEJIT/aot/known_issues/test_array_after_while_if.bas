REM Minimal test case for array access bug after WHILE with nested IF

DIM arr(100) AS INT
DIM i AS INT

REM Initialize array
i = 1
WHILE i <= 10
    arr(i) = i * 10
    i = i + 1
WEND

REM Simple IF statement
IF arr(1) = 10 THEN
    PRINT "arr(1) is correct: "; arr(1)
END IF

REM This should work but crashes
PRINT "After IF, arr(1) = "; arr(1)
PRINT "arr(2) = "; arr(2)
PRINT "arr(3) = "; arr(3)
