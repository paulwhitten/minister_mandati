def summarize(classes):
    """Average score per student, per class."""
    result = {}
    for name, students in classes.items():
        averages = {}
        for student, scores in students.items():
            if student:
                total = sum(scores)
                averages[student] = total / len(scores)
        result[name] = averages
    return result
